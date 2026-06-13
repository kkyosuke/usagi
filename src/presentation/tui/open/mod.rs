//! Project selection screen (画面 #2).
//!
//! Lists the registered workspaces (most recently used first) and lets the
//! user pick one to open. Selecting a project is a placeholder for now — the
//! workspace screen it should lead to is not implemented yet — so it shows a
//! "coming soon" notice.

pub mod event;
pub mod state;
pub mod ui;

use std::io;

use anyhow::Result;
use console::{Key, Term};

use crate::domain::workspace::Workspace;
use crate::infrastructure::storage::Storage;
use crate::presentation::tui::screen::KeyReader;
use crate::usecase::workspace;

pub use event::Outcome;

use state::ProjectList;

/// Reads keys from a real terminal.
struct TermKeyReader {
    term: Term,
}

impl KeyReader for TermKeyReader {
    fn read_key(&mut self) -> io::Result<Key> {
        self.term.read_key()
    }
}

/// Runs the project selection screen on the given terminal until the user goes
/// back or quits. Wires the real terminal and storage-backed workspace list to
/// the testable event loop in [`event`]. Assumes the alternate screen is
/// already active.
pub fn run(term: &Term) -> Result<Outcome> {
    let (list, notice) = match load_workspaces() {
        Ok(workspaces) => (ProjectList::new(workspaces), None),
        Err(e) => (
            ProjectList::new(Vec::new()),
            Some(format!("Failed to load projects: {e}")),
        ),
    };
    let mut reader = TermKeyReader { term: term.clone() };
    event::event_loop(term, &mut reader, list, notice)
}

/// Loads the registered workspaces, most recently used first.
fn load_workspaces() -> Result<Vec<Workspace>> {
    let storage = Storage::open_default()?;
    workspace::list(&storage)
}
