mod event;
mod menu;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::infrastructure::storage::Storage;
use crate::presentation::tui::config;
use crate::presentation::tui::new;
use crate::presentation::tui::new::state::NewProject;
use crate::presentation::tui::open;
use crate::presentation::tui::term_reader::TermKeyReader;
use crate::usecase::project;

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

    let mut reader = TermKeyReader::new(term.clone());
    let mut open_open = |t: &Term| open::run(t);
    let mut open_new = |t: &Term| new::run(t, &default_location);
    let mut create_project = |form: &NewProject| -> Result<Workspace> {
        match form {
            NewProject::Clone(spec) => project::create(
                &storage,
                &spec.url,
                &spec.location,
                &spec.directory,
                spec.branch.as_deref(),
            ),
            NewProject::Existing(spec) => {
                project::register_existing(&storage, &spec.path, &spec.name)
            }
        }
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
