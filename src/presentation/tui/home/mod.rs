//! Home screen (画面 #5, workspace view).
//!
//! Opened after a workspace is chosen on the project selection screen. Shows
//! the workspace's worktrees, loaded from its `<workspace>/.usagi/state.json`,
//! and lets the user pick one. Acting on a worktree is a placeholder for now —
//! the per-worktree session screen is not implemented yet — so selecting one
//! shows a "coming soon" notice.

pub mod command;
pub mod event;
pub mod state;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::infrastructure::history_store::HistoryStore;
use crate::infrastructure::workspace_store::WorkspaceStore;
use crate::presentation::tui::term_reader::TermKeyReader;

pub use event::Outcome;

use state::{HomeState, LogLine, SessionOutcome};

/// Runs the home screen for `workspace` on the given terminal until the user
/// goes back or quits. Loads the workspace's worktree state and prior command
/// history from disk and wires it, with the real terminal, to the testable
/// event loop in [`event`]. Each command the user runs is appended to the
/// workspace's `history.json` (best-effort). Assumes the alternate screen is
/// already active (it is owned by the orchestrator).
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
    state.restore_sessions(sessions);

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

    // Creating a session does the git / filesystem work and reports back. When
    // the workspace root is a single repository, re-syncing yields the refreshed
    // worktree list (including the new session); a multi-repo root has no single
    // `state.json` to sync, so `sync` fails harmlessly and the list is unchanged.
    let root = workspace.path.clone();
    let mut create_session = |name: &str| match crate::usecase::session::create(&root, name) {
        Ok(created) => SessionOutcome {
            line: LogLine::output(format!(
                "Created session \"{}\" ({} worktree(s)) 🐰",
                created.name,
                created.worktrees.len()
            )),
            worktrees: crate::usecase::workspace_state::sync(&root)
                .ok()
                .map(|s| s.worktrees),
            // Sessions are recorded in state.json by `create`; reload them so
            // `session list` reflects the new one (works for multi-repo too).
            sessions: WorkspaceStore::new(&root)
                .load()
                .ok()
                .flatten()
                .map(|s| s.sessions),
        },
        Err(e) => SessionOutcome {
            line: LogLine::error(format!("session failed: {e}")),
            worktrees: None,
            sessions: None,
        },
    };

    event::event_loop(term, &mut reader, state, &mut persist, &mut create_session)
}
