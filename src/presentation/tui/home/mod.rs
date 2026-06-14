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

use std::path::Path;

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::domain::workspace_state::SessionRecord;
use crate::infrastructure::history_store::HistoryStore;
use crate::infrastructure::workspace_store::WorkspaceStore;
use crate::presentation::tui::term_reader::TermKeyReader;

pub use event::Outcome;

use state::{HomeState, LogLine, SessionOutcome};

/// Refresh the workspace's session state from git (best-effort) and return the
/// sessions to show. `sync` rewrites each session worktree's status; for a
/// non-git root it fails harmlessly, so we fall back to the saved sessions.
fn reload_sessions(root: &Path) -> Option<Vec<SessionRecord>> {
    if let Ok(state) = crate::usecase::workspace_state::sync(root) {
        return Some(state.sessions);
    }
    WorkspaceStore::new(root)
        .load()
        .ok()
        .flatten()
        .map(|s| s.sessions)
}

/// Runs the home screen for `workspace` on the given terminal until the user
/// goes back or quits. Loads the workspace's worktree state and prior command
/// history from disk and wires it, with the real terminal, to the testable
/// event loop in [`event`]. Each command the user runs is appended to the
/// workspace's `history.json` (best-effort). Assumes the alternate screen is
/// already active (it is owned by the orchestrator).
pub fn run(term: &Term, workspace: &Workspace) -> Result<Outcome> {
    let (sessions, notice) = match WorkspaceStore::new(&workspace.path).load() {
        Ok(Some(state)) => (state.sessions, None),
        Ok(None) => (Vec::new(), None),
        Err(e) => (Vec::new(), Some(format!("Failed to load sessions: {e}"))),
    };
    let mut state = HomeState::new(workspace.name.clone(), Vec::new(), notice);
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

    // Creating a session does the git / filesystem work and reports back. The
    // refreshed sessions (with each worktree's git status) are read back so the
    // worktree pane and `session list` reflect the new session.
    let root = workspace.path.clone();
    let mut create_session = |name: &str| match crate::usecase::session::create(&root, name) {
        Ok(created) => SessionOutcome {
            line: LogLine::output(format!(
                "Created session \"{}\" ({} worktree(s)) 🐰",
                created.name,
                created.worktrees.len()
            )),
            sessions: reload_sessions(&root),
        },
        Err(e) => SessionOutcome {
            line: LogLine::error(format!("session failed: {e}")),
            sessions: None,
        },
    };

    // Removing a session deletes its worktrees/branches and forgets it. A
    // session with uncommitted changes is left untouched unless `--force`.
    let remove_root = workspace.path.clone();
    let mut remove_session = |name: &str, force: bool| match crate::usecase::session::remove(
        &remove_root,
        name,
        force,
    ) {
        Ok(outcome) if outcome.removed => SessionOutcome {
            line: LogLine::output(format!("Removed session \"{name}\" 🧹")),
            sessions: reload_sessions(&remove_root),
        },
        Ok(outcome) => {
            let paths = outcome
                .dirty
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            SessionOutcome {
                line: LogLine::error(format!(
                    "session \"{name}\" has uncommitted changes ({paths}). \
                         Use \"session remove {name} --force\" to discard."
                )),
                sessions: None,
            }
        }
        Err(e) => SessionOutcome {
            line: LogLine::error(format!("session remove failed: {e}")),
            sessions: None,
        },
    };

    event::event_loop(
        term,
        &mut reader,
        state,
        &mut persist,
        &mut create_session,
        &mut remove_session,
    )
}
