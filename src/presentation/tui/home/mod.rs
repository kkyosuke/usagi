//! Home screen (画面 #5, workspace view).
//!
//! Opened after a workspace is chosen on the project selection screen. Shows
//! the workspace's worktrees — those synced from git plus any session worktrees
//! — loaded from its `<workspace>/.usagi/state.json`. From the command line the
//! user can create sessions (`session new`), list them (`session list`), and
//! drop into an interactive shell rooted at the active worktree (`terminal`).

pub mod command;
pub mod event;
pub mod state;
pub mod ui;

use std::path::Path;

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::infrastructure::history_store::HistoryStore;
use crate::infrastructure::terminal;
use crate::infrastructure::workspace_store::WorkspaceStore;
use crate::presentation::tui::term_reader::TermKeyReader;
use crate::usecase::session;

use event::HomeHandlers;
pub use event::Outcome;

use state::HomeState;

/// Runs the home screen for `workspace` on the given terminal until the user
/// goes back or quits. Loads the workspace's worktree state, session records,
/// and prior command history from disk and wires it, with the real terminal, to
/// the testable event loop in [`event`]. Each command the user runs is appended
/// to the workspace's `history.json` (best-effort). Assumes the alternate screen
/// is already active (it is owned by the orchestrator).
pub fn run(term: &Term, workspace: &Workspace) -> Result<Outcome> {
    let (worktrees, sessions, notice) = match WorkspaceStore::new(&workspace.path).load() {
        Ok(Some(state)) => (state.worktrees, state.sessions, None),
        Ok(None) => (Vec::new(), Vec::new(), None),
        Err(e) => (
            Vec::new(),
            Vec::new(),
            Some(format!("Failed to load worktrees: {e}")),
        ),
    };
    let mut state = HomeState::new(workspace.name.clone(), worktrees, notice);
    // Surface previously created sessions' worktrees in the sidebar too.
    for recorded in &sessions {
        state.add_worktrees(event::session_rows(recorded));
    }

    // Restore past commands so `history` and `↑`/`↓` recall span sessions.
    // A read failure is non-fatal: just start with an empty history.
    let history = HistoryStore::new(&workspace.path);
    if let Ok(entries) = history.load() {
        state.restore_history(entries.into_iter().map(|e| e.command).collect());
    }

    let mut reader = TermKeyReader::new(term.clone());
    // Persisting a command is best-effort; a write failure must not break the
    // screen, so the error is intentionally dropped (cf. `hop`'s notification).
    let mut persist = |command: &str| {
        let _ = history.append(command);
    };

    // Side-effect handlers: create / list sessions through the usecase, and open
    // a shell by temporarily leaving the alternate screen and restoring it once
    // the user exits the shell.
    let mut create_session = |name: &str| session::create(&workspace.path, name);
    let mut list_sessions = || session::list(&workspace.path);
    let mut open_terminal = |dir: &Path| -> Result<()> {
        term.write_str("\x1b[?1049l")?;
        term.show_cursor()?;
        let result = terminal::open(dir);
        term.write_str("\x1b[?1049h")?;
        term.hide_cursor()?;
        result
    };
    let mut handlers = HomeHandlers {
        workspace_root: workspace.path.clone(),
        create_session: &mut create_session,
        list_sessions: &mut list_sessions,
        open_terminal: &mut open_terminal,
    };

    event::event_loop(term, &mut reader, state, &mut persist, &mut handlers)
}
