//! Home screen (画面 #5, workspace view).
//!
//! Opened after a workspace is chosen on the project selection screen. Shows
//! the workspace's worktrees, loaded from its `<workspace>/.usagi/state.json`,
//! and lets the user pick one. Acting on a worktree is a placeholder for now —
//! the per-worktree session screen is not implemented yet — so selecting one
//! shows a "coming soon" notice.

pub mod event;
pub mod state;
pub mod ui;

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::infrastructure::workspace_store::WorkspaceStore;
use crate::presentation::tui::term_reader::TermKeyReader;

pub use event::Outcome;

use state::WorktreeList;

/// Runs the home screen for `workspace` on the given terminal until the user
/// goes back or quits. Loads the workspace's worktree state from disk and wires
/// it, with the real terminal, to the testable event loop in [`event`]. Assumes
/// the alternate screen is already active (it is owned by the welcome screen).
pub fn run(term: &Term, workspace: &Workspace) -> Result<Outcome> {
    let (list, notice) = match WorkspaceStore::new(&workspace.path).load() {
        Ok(Some(state)) => (
            WorktreeList::new(workspace.name.clone(), state.worktrees),
            None,
        ),
        Ok(None) => (WorktreeList::new(workspace.name.clone(), Vec::new()), None),
        Err(e) => (
            WorktreeList::new(workspace.name.clone(), Vec::new()),
            Some(format!("Failed to load worktrees: {e}")),
        ),
    };
    let mut reader = TermKeyReader::new(term.clone());
    event::event_loop(term, &mut reader, list, notice)
}
