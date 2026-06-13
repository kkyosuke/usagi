mod event;
mod menu;
pub mod ui;

use std::io;

use anyhow::Result;
use console::{Key, Term};

use crate::infrastructure::storage::Storage;
use crate::presentation::tui::config;
use crate::presentation::tui::new;
use crate::presentation::tui::new::state::NewProject;
use crate::presentation::tui::open;
use crate::presentation::tui::screen::KeyReader;
use crate::usecase::project;

/// Reads keys from a real terminal.
struct TermKeyReader {
    term: Term,
}

impl KeyReader for TermKeyReader {
    fn read_key(&mut self) -> io::Result<Key> {
        // `read_key_raw` surfaces Ctrl+C as `Key::CtrlC` instead of raising
        // SIGINT, so the event loop can quit gracefully and the alternate
        // screen guard restores the terminal on the way out.
        self.term.read_key_raw()
    }
}

/// Displays the welcome screen and dispatches the selected menu action.
///
/// `Open` opens the project selection screen and `New` opens the New Project
/// screen; submitting the New form clones the repository, registers it as a
/// workspace, and opens it. `Config` opens the configuration screen. This
/// function wires the real terminal, the real sub-screens, and the project use
/// case to the testable event loop in [`event`].
pub fn run() -> Result<()> {
    let term = Term::stdout();
    let storage = Storage::open_default()?;
    // Pre-fill the New form's Location field with the configured base directory.
    let default_location = project::default_location(&storage)?
        .to_string_lossy()
        .into_owned();

    let mut reader = TermKeyReader { term: term.clone() };
    let mut open_open = |t: &Term| open::run(t);
    let mut open_new = |t: &Term| new::run(t, &default_location);
    let mut create_project = |form: &NewProject| {
        project::create(
            &storage,
            &form.url,
            &form.location,
            &form.directory,
            form.branch.as_deref(),
        )
    };
    let mut open_config = |t: &Term| config::run(t);
    event::event_loop(
        &term,
        &mut reader,
        &mut open_open,
        &mut open_new,
        &mut create_project,
        &mut open_config,
    )
}
