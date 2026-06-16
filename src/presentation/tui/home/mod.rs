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
pub mod terminal_pane;
pub mod terminal_pool;
pub mod terminal_view;
pub mod ui;
pub mod update;

#[cfg(test)]
mod e2e_tests;

use std::path::Path;

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::domain::workspace_state::SessionRecord;
use crate::infrastructure::history_store::HistoryStore;
use crate::infrastructure::workspace_store::WorkspaceStore;
use crate::presentation::tui::term_reader::TermKeyReader;

pub use event::Outcome;

use state::{HomeState, LogLine, PaneExit, SessionOutcome};

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
    // Sync from git on entry so the worktree statuses are current the moment the
    // screen opens (a branch may have been committed / pushed / merged since the
    // last visit). A non-git root or a sync failure falls back to the saved
    // sessions, mirroring `reload_sessions`.
    let (sessions, notice) = match crate::usecase::workspace_state::sync(&workspace.path) {
        Ok(state) => (state.sessions, None),
        Err(_) => match WorkspaceStore::new(&workspace.path).load() {
            Ok(Some(state)) => (state.sessions, None),
            Ok(None) => (Vec::new(), None),
            Err(e) => (Vec::new(), Some(format!("Failed to load sessions: {e}"))),
        },
    };
    let mut state = HomeState::new(workspace.name.clone(), Vec::new(), notice);
    state.restore_sessions(sessions);

    // Which right-pane action surface 在席 (Focus) presents — a pickable menu or
    // a typed prompt — from the effective settings (project-local over the global
    // default). Any failure to read settings falls back to the default (Menu).
    let session_action_ui = crate::infrastructure::storage::Storage::open_default()
        .and_then(|storage| crate::usecase::settings::effective(&storage, &workspace.path))
        .map(|settings| settings.session_action_ui)
        .unwrap_or_default();
    state.set_session_action_ui(session_action_ui);

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
            // Select the freshly created session so it is active straight away.
            select: Some(created.name),
        },
        Err(e) => SessionOutcome {
            line: LogLine::error(format!("session failed: {e}")),
            sessions: None,
            select: None,
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
            select: None,
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
                select: None,
            }
        }
        Err(e) => SessionOutcome {
            line: LogLine::error(format!("session remove failed: {e}")),
            sessions: None,
            select: None,
        },
    };

    // The agent CLI launched by `:agent`, resolved from the effective settings
    // (project-local overrides on top of the global default, which is Claude).
    // The launch command wires in usagi's issue MCP server (where the agent CLI
    // supports it) so the agent can manage issues from the start, plus the local
    // LLM server when it is enabled. Any failure to read settings falls back to
    // the default agent.
    let agent_command = crate::infrastructure::storage::Storage::open_default()
        .and_then(|storage| crate::usecase::settings::effective(&storage, &workspace.path))
        .map(|settings| settings.agent_launch_command())
        .unwrap_or_else(|_| crate::domain::settings::Settings::default().agent_launch_command());

    // Whether to surface desktop notifications when a background session starts
    // waiting for input, from the effective settings (project-local over the
    // global default). Any failure to read settings defaults to enabled, like
    // `hop`'s welcome notification.
    let notifications_enabled = crate::infrastructure::storage::Storage::open_default()
        .and_then(|storage| crate::usecase::settings::effective(&storage, &workspace.path))
        .map(|settings| settings.notifications_enabled)
        .unwrap_or(true);

    // The live shells embedded in the right pane, one per worktree, kept alive
    // across session switches and for as long as this screen is open. Dropped on
    // return, which kills any shell still running. The pool also watches every
    // shell's bell and flags / notifies the ones waiting for input.
    //
    // Wrapped in a `RefCell` so both the pane driver (`open_terminal`) and the
    // sidebar preview (`preview`) can reach it: their borrows never overlap in
    // time (the event loop calls one or the other, never both at once).
    let pool = std::cell::RefCell::new(terminal_pool::TerminalPool::new(notifications_enabled));
    let monitor = pool.borrow().monitor();

    // Check the project's git remote for a newer release than this build, on a
    // background thread so a slow or unreachable network never delays the screen.
    // The result is written to the handle the event loop reads each redraw; when
    // a newer version is published it surfaces the top-right "update available"
    // notice. Any failure (offline, git missing, already up to date) simply
    // leaves the handle empty and the notice hidden.
    let update = update::UpdateHandle::new();
    {
        let handle = update.clone();
        std::thread::spawn(move || {
            if let Some(status) =
                crate::usecase::update_check::check(env!("CARGO_PKG_VERSION"), || {
                    crate::infrastructure::release::fetch_tags(env!("CARGO_PKG_REPOSITORY"))
                })
            {
                handle.set(status);
            }
        });
    }

    // Opening a terminal embeds a live shell in the right pane: the pane stays
    // inside the workspace screen (sidebar still visible) and runs the shell
    // until the user detaches, switches sessions, or it exits. `:agent` is the
    // same, with the agent CLI sent to the shell on its first spawn. The pool
    // owns the shell so a detach leaves it running; the right-pane mode and the
    // switch loop are handled by the event loop around this call. The attached
    // session is declared to the monitor (so it is never flagged as waiting) and
    // cleared again on detach / close.
    let terminal_root = workspace.path.clone();
    let mut open_terminal = |home: &mut HomeState, dir: &Path, agent: bool| -> Result<PaneExit> {
        let initial = agent.then_some(agent_command.as_str());
        let label = home
            .list()
            .worktrees()
            .iter()
            .find(|w| w.path.as_path() == dir)
            .map(state::worktree_name)
            .map(str::to_string)
            .unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.display().to_string())
            });
        let mut pool = pool.borrow_mut();
        let handle = pool.monitor();
        let pty = pool.attach_or_spawn(term, dir, initial, &label)?;
        handle.set_attached(Some(dir.to_path_buf()));
        let result = terminal_pane::run(term, home, pty, &handle);
        // Leaving the pane (Ctrl-O → 切替, the shell closing, or an error) means
        // nothing is attached any more; the shell itself stays alive in the pool.
        handle.set_attached(None);
        // The user may have committed / pushed / merged while in the pane, so
        // re-sync the worktree statuses now that they have left it — keeping the
        // cursor where it is. Best-effort: a sync failure just leaves the
        // last-known statuses in place.
        if let Some(sessions) = reload_sessions(&terminal_root) {
            home.refresh_sessions(sessions);
        }
        result
    };

    // Snapshot the selected session's live terminal for the sidebar's right-pane
    // preview (the tab-like view), or `None` when it has no running shell/agent.
    let mut preview =
        |dir: &Path| -> Option<crate::presentation::tui::home::terminal_view::TerminalView> {
            pool.borrow_mut().snapshot(term, dir)
        };

    // Opening `config` hands off to the settings screen in its workspace scope,
    // editing only this workspace's local overrides
    // (`<workspace>/.usagi/settings.json`); the global settings are changed from
    // the CLI or welcome menu instead. Quitting there (Ctrl+C) quits the app,
    // reported back as `true` so the event loop propagates the quit; `Back`
    // returns `false`.
    let config_root = workspace.path.clone();
    let mut open_config = |t: &Term| -> Result<bool> {
        match crate::presentation::tui::config::run_in(t, Some(config_root.clone()))? {
            crate::presentation::tui::config::Outcome::Back => Ok(false),
            crate::presentation::tui::config::Outcome::Quit => Ok(true),
        }
    };

    event::event_loop(
        term,
        &mut reader,
        state,
        &workspace.path,
        &monitor,
        &update,
        &mut persist,
        &mut create_session,
        &mut remove_session,
        &mut open_terminal,
        &mut open_config,
        &mut preview,
    )
}
