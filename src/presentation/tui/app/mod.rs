//! Interactive TUI orchestrator.
//!
//! Owns the screen-graph navigation for `usagi hop`: it runs the welcome menu
//! and, based on the chosen action, opens the project selection, New Project,
//! or Config screens, creates a project and opens its home screen, and routes
//! each sub-screen's Back/Quit/error outcome. Individual screens stay pure —
//! they only render and report what the user chose; this module decides what
//! those choices mean.

pub mod event;

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::infrastructure::storage::Storage;
use crate::presentation::tui::new::state::NewProject;
use crate::presentation::tui::{config, home, new, open, splash, welcome};
use crate::usecase::project;

/// Entry point for the interactive TUI. Wires the real terminal, storage, and
/// screens to the testable [`event::event_loop`], which owns the
/// alternate-screen lifetime for the whole session.
pub fn run() -> Result<()> {
    let term = Term::stdout();
    let storage = Storage::open_default()?;
    // Pre-fill the New form's Location field with the configured base directory.
    let default_location = project::default_location(&storage)?
        .to_string_lossy()
        .into_owned();

    let mut run_splash = |t: &Term| splash::run(t);
    let mut run_welcome = |t: &Term, notice: Option<String>| welcome::run(t, notice);
    let mut run_open = |t: &Term| open::run(t);
    let mut run_new = |t: &Term| new::run(t, &default_location);
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
    let mut run_home = |t: &Term, ws: &Workspace| home::run(t, ws);
    let mut run_config = |t: &Term| config::run(t);

    event::event_loop(
        &term,
        &mut run_splash,
        &mut run_welcome,
        &mut run_open,
        &mut run_new,
        &mut create_project,
        &mut run_home,
        &mut run_config,
    )
}
