use super::super::terminal::pool::MonitorSnapshot;
use super::*;
use crate::domain::workspace_state::BranchStatus;
use chrono::Utc;
use std::path::PathBuf;

fn worktree(branch: &str) -> WorktreeState {
    WorktreeState {
        branch: Some(branch.to_string()),
        path: PathBuf::from(format!("/repo/{branch}")),
        head: "abc1234".to_string(),
        primary: false,
        upstream: None,
        status: BranchStatus::Local,
        diff: None,
        ahead_behind: None,
        updated_at: Utc::now(),
    }
}

fn sample() -> WorktreeList {
    WorktreeList::new(
        "usagi",
        vec![worktree("main"), worktree("feature"), worktree("fix")],
    )
}

// --- shared test helpers ---
fn state() -> HomeState {
    HomeState::new("usagi", vec![worktree("main"), worktree("feature")], None)
}

/// A [`Logger`](crate::infrastructure::error_log::Logger) that captures every
/// recorded message, so a test can assert which on-screen errors are persisted.
/// The shared `Rc<RefCell<…>>` lets the test read what the injected sink received.
#[derive(Clone, Default)]
struct SpyLogger {
    recorded: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
}

impl crate::infrastructure::error_log::Logger for SpyLogger {
    fn record(&self, message: &str) {
        self.recorded.borrow_mut().push(message.to_string());
    }
}

/// A [`HomeState`] wired to a [`SpyLogger`], returning both so the test can drive
/// the screen and inspect what was recorded.
fn state_with_spy() -> (HomeState, SpyLogger) {
    let spy = SpyLogger::default();
    let mut state = state();
    state.set_logger(Box::new(spy.clone()));
    (state, spy)
}
fn session_record(name: &str, worktrees: usize) -> SessionRecord {
    SessionRecord {
        name: name.to_string(),
        display_name: None,
        note: None,
        root: std::path::PathBuf::from(format!("/repo/.usagi/sessions/{name}")),
        worktrees: (0..worktrees).map(|_| worktree(name)).collect(),
        created_at: Utc::now(),
        last_active: None,
    }
}

/// A state with two sessions recorded, the cursor moved onto the first one
/// (`alpha`), so the note-editor helpers act on a real session row.
fn state_on_alpha() -> HomeState {
    let mut state = state();
    let mut alpha = session_record("alpha", 1);
    alpha.note = Some("existing".to_string());
    state.restore_sessions(vec![alpha, session_record("beta", 1)]);
    state.switch_move_down(); // root -> alpha
    state
}

mod attached;
mod caret_switch;
mod focus;
mod homestate;
mod note_editor;
mod worktree_list;
