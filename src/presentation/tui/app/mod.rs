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
use crate::presentation::tui::io::{screen, signals};
use crate::presentation::tui::new::state::NewProject;
use crate::presentation::tui::{config, home, new, open, splash, welcome};
use crate::usecase::project;

/// Best-effort terminal reset on panic, installed once before the TUI starts.
///
/// The RAII guards (`AlternateScreenGuard` and the embedded pane's mode guard)
/// already restore the terminal when a panic unwinds through them, so this is the
/// last line of defense — it runs even if unwinding is disabled or a panic
/// escapes a path no guard covers, so the user is never left in raw mode with a
/// hidden cursor and live mouse reporting. It chains to the previous hook so the
/// panic message still prints.
///
/// The restore bytes it writes are the shared [`screen::TERMINAL_RESTORE`]
/// sequence — the same ones the signal handlers ([`signals`]) write on an abrupt
/// exit — so the two Drop-less exit paths stay in lock-step.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        // Leave the alternate screen; disable mouse click/drag/motion reporting
        // and bracketed paste; show the cursor.
        let _ = screen::write_terminal_restore(&mut std::io::stdout());
        previous(info);
    }));
}

/// Entry point for the interactive TUI. Wires the real terminal, storage, and
/// screens to the testable [`event::event_loop`], which owns the
/// alternate-screen lifetime for the whole session.
pub fn run() -> Result<()> {
    install_panic_hook();
    // Restore the terminal on a signal that skips the RAII guards (a real SIGINT
    // that beats `Key::CtrlC`, `kill`/SIGTERM, or a closed terminal/SIGHUP), so
    // the shell is never left echoing mouse reports. See [`signals`].
    signals::install();
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
    // Jumping straight into a freshly created project's home screen: load its
    // (minimal) workspace data synchronously — there is no list-hiding animation
    // to overlap here, unlike the Open screen path (see [`open::run`]).
    let mut run_home =
        |t: &Term, ws: &Workspace| home::run(t, std::slice::from_ref(ws), home::preload(ws));
    let mut run_recent = |t: &Term, ws: &Workspace| open::open_home(t, std::slice::from_ref(ws));
    let mut run_config = |t: &Term| config::run(t);

    event::event_loop(
        &term,
        &mut run_splash,
        &mut run_welcome,
        &mut run_open,
        &mut run_new,
        &mut create_project,
        &mut run_home,
        &mut run_recent,
        &mut run_config,
    )
}
