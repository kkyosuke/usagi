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
pub mod tasks;
pub mod terminal_link;
pub mod terminal_pane;
pub mod terminal_pool;
pub mod terminal_selection;
pub mod terminal_tabs;
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

/// Lock the session op-lock, recovering from a poisoned mutex. The guarded value
/// is `()` — a worker that panicked while holding it left no invalid state behind
/// — so recovering keeps session create / remove / rename working instead of
/// bricking the feature: a poisoned lock would otherwise panic every later
/// dispatch's worker thread, leaving its task row spinning forever.
fn lock_session_ops(lock: &std::sync::Mutex<()>) -> std::sync::MutexGuard<'_, ()> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run a session worker's body under [`catch_unwind`](std::panic::catch_unwind)
/// so a panic inside the git / filesystem work no longer vanishes with the dead
/// thread. A panicked worker would otherwise never call
/// [`complete`](tasks::TaskHandle::complete) — leaving its task row spinning
/// forever — and the panic that poisoned the op-lock (recovered blindly by
/// [`lock_session_ops`]) would leave no trace. On a panic this records the
/// payload to the error log and settles the row as failed. The message / row
/// wording is built in [`tasks::panic_outcome`], where it is tested; the spawn
/// and the unwind boundary stay here in the coverage-excluded home module.
fn complete_or_record_panic(
    handle: &tasks::TaskHandle,
    id: u64,
    kind: tasks::TaskKind,
    target: &str,
    work: impl FnOnce() -> (bool, tasks::Completion),
) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok((ok, completion)) => handle.complete(id, ok, completion),
        Err(payload) => {
            let (log_line, completion) = tasks::panic_outcome(kind, target, payload);
            crate::infrastructure::error_log::ErrorLog::record(&log_line);
            handle.complete(id, false, completion);
        }
    }
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
    // The root row (`⌂ root`) operates in the workspace root; record its path so
    // the 切替 preview can recognise the root's live embedded session (keyed by
    // this path) and show its terminal, mirroring how worktree rows are matched.
    state.set_root_path(workspace.path.clone());
    state.restore_sessions(sessions);

    // Load the workspace's task issues so the `issue` command can list / graph /
    // show them. A read failure is non-fatal: the command just shows none.
    if let Ok(issues) = crate::infrastructure::issue_store::IssueStore::new(&workspace.path).scan()
    {
        state.set_issues(issues);
    }

    // Which right-pane action surface 在席 (Focus) presents — a pickable menu or
    // a typed prompt — from the effective settings (project-local over the global
    // default). Re-read again whenever the config screen closes (see
    // `open_config`) so a change takes effect without reopening this screen.
    state.set_session_action_ui(effective_settings(&workspace.path).session_action_ui);

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

    // The background session tasks (create / remove) the event loop dispatches
    // and renders in the top-right task panel, shared with the worker threads.
    let tasks = tasks::TaskHandle::new();
    // Serialises the session-mutating git work across worker threads: both
    // create and remove load-modify-save `state.json`, so concurrent runs would
    // race. Each worker holds this for the duration of its git work, so a burst
    // of dispatches runs one at a time (all shown in the panel) without freezing
    // the event loop.
    let op_lock = std::sync::Arc::new(std::sync::Mutex::new(()));

    // Join handles for the spawned session workers. The event loop waits on these
    // when it exits (below) so an in-flight create / remove finishes its git work
    // instead of the process killing the thread mid-`worktree add` / `remove` and
    // leaving a half-written worktree or `state.json`. Single-threaded: only the
    // event loop, through the dispatch closures one at a time, ever touches it,
    // so a plain `Rc<RefCell<_>>` suffices (like `pool` below).
    let workers: std::rc::Rc<std::cell::RefCell<Vec<std::thread::JoinHandle<()>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));

    // Creating a session does the git / filesystem work on a background thread so
    // the screen never freezes: it registers a task row, runs the work under the
    // op-lock, and stores the result for the event loop to drain (logging it and
    // refreshing the pane). Returns the moment the thread is spawned.
    let create_tasks = tasks.clone();
    let create_root = workspace.path.clone();
    let create_lock = op_lock.clone();
    let create_workers = workers.clone();
    let mut dispatch_create = move |name: &str| {
        let id = create_tasks.begin(tasks::TaskKind::CreateSession, name);
        let handle = create_tasks.clone();
        let root = create_root.clone();
        let name = name.to_string();
        let lock = create_lock.clone();
        let worker = std::thread::spawn(move || {
            complete_or_record_panic(&handle, id, tasks::TaskKind::CreateSession, &name, || {
                let _guard = lock_session_ops(&lock);
                run_create(&root, &name)
            });
        });
        create_workers.borrow_mut().push(worker);
    };

    // The branch names already taken across the workspace, read fresh each time
    // the inline create input opens so the typed name can be validated live
    // against duplicates and branch-namespace clashes.
    let branches_root = workspace.path.clone();
    let mut existing_branches =
        move || crate::usecase::session::existing_branch_names(&branches_root);

    // Renaming a session's sidebar label persists the new display name to
    // state.json and re-reads the sessions so the pane reflects it. The branch /
    // identity is untouched, so the renamed session keeps its row: `select` holds
    // its name to keep the cursor on it after the list rebuilds.
    //
    // Unlike create / remove this stays synchronous (no git work to block on),
    // but it still load-modify-saves `state.json`, so it takes the same op-lock
    // to serialise against the background workers — otherwise a rename landing
    // mid-`worktree add` would be clobbered by the worker's later write. The lock
    // is only contended while a background op is genuinely in flight, so the
    // momentary wait is bounded to exactly the window where serialising matters.
    let rename_root = workspace.path.clone();
    let rename_lock = op_lock.clone();
    let mut rename_display = |name: &str, label: &str| {
        let _guard = lock_session_ops(&rename_lock);
        match crate::usecase::session::set_display_name(&rename_root, name, label) {
            Ok(shown) => SessionOutcome {
                line: LogLine::output(format!("Renamed \"{name}\" to \"{shown}\" 🏷")),
                sessions: reload_sessions(&rename_root),
                select: Some(name.to_string()),
            },
            Err(e) => SessionOutcome {
                line: LogLine::error(format!("rename failed: {e}")),
                sessions: None,
                select: None,
            },
        }
    };

    // The effective settings for this workspace (project-local overrides on top
    // of the global default), read once. Any failure falls back to the defaults.
    let settings = crate::infrastructure::storage::Storage::open_default()
        .and_then(|storage| crate::usecase::settings::effective(&storage, &workspace.path))
        .unwrap_or_default();

    // Whether the 在席 (Focus) menu offers the `ai` command: only when the local
    // LLM is enabled and its model is pulled, so it appears only when running it
    // would actually work. Probed once here (an `ollama show`) and re-probed when
    // the config screen closes (see `open_config`).
    state.set_ai_available(local_llm_available(&settings));

    // The wired-in MCP servers and lifecycle hooks invoke usagi back, so they are
    // pointed at this process's own executable path rather than the bare name
    // `usagi`: that way they resolve even when usagi is run straight from a build
    // (`cargo run`) and is not installed on `$PATH`. If the path can't be
    // determined we fall back to the bare name.
    let usagi_bin = std::env::current_exe()
        .ok()
        .and_then(|path| path.into_os_string().into_string().ok())
        .unwrap_or_else(|| "usagi".to_string());

    // The agent adapter `:agent` drives, picked from the configured CLI — the
    // single source of the launch command built per worktree below.
    let agent = crate::infrastructure::agent::agent_for(settings.agent_cli);

    // usagi's wiring policy (resolved usagi binary + local LLM model) the agent
    // renders into its own invocation; built once and reused for every launch.
    let agent_wiring = settings.agent_wiring(&usagi_bin);

    // Whether to surface desktop notifications when a background session starts
    // waiting for input or finishes. Opt-out: on unless the user disabled it.
    let notifications_enabled = settings.notifications_enabled;

    // The live shells embedded in the right pane, one per worktree, kept alive
    // across session switches and for as long as this screen is open. Dropped on
    // return, which kills any shell still running. The pool also watches every
    // shell's bell/phase and flags / notifies the ones waiting or finished.
    //
    // Wrapped in a `RefCell` so the pane driver (`open_terminal`), the sidebar
    // preview (`preview`), and `remove_session` (which evicts a removed session's
    // shell) can all reach it: their borrows never overlap in time (the event
    // loop calls one at a time).
    let pool = std::cell::RefCell::new(terminal_pool::TerminalPool::new(notifications_enabled));
    let monitor = pool.borrow().monitor();

    // Removing a session deletes its worktrees/branches and forgets it, on a
    // background thread like creation so the screen never freezes. A session with
    // uncommitted changes is left untouched unless `--force`. The git work runs
    // under the op-lock; the result (and, on success, the pool path whose shell
    // to evict) is stored for the event loop to drain.
    let remove_tasks = tasks.clone();
    let remove_root = workspace.path.clone();
    let remove_lock = op_lock.clone();
    let remove_agent = agent.clone();
    let remove_workers = workers.clone();
    let mut dispatch_remove = move |name: &str, force: bool| {
        let id = remove_tasks.begin(tasks::TaskKind::RemoveSession, name);
        let handle = remove_tasks.clone();
        let root = remove_root.clone();
        let name = name.to_string();
        let lock = remove_lock.clone();
        let agent = remove_agent.clone();
        let worker = std::thread::spawn(move || {
            complete_or_record_panic(&handle, id, tasks::TaskKind::RemoveSession, &name, || {
                let _guard = lock_session_ops(&lock);
                run_remove(&root, &name, force, agent.as_ref())
            });
        });
        remove_workers.borrow_mut().push(worker);
    };

    // Evict a removed session's still-running shell from the pool so a session
    // later recreated at the same path starts fresh instead of re-attaching to
    // this run's agent and its history. Run by the event loop when it drains a
    // finished removal — on this thread, since the pool is not `Send` and so
    // cannot be touched from the worker thread.
    let mut evict_pool = |session_root: &Path| {
        pool.borrow_mut().remove_under(session_root);
    };

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
    // owns the shells so a detach leaves them running; the right-pane mode and
    // the switch loop are handled by the event loop around this call.
    //
    // A session can hold several panes (an agent alongside terminals): this loop
    // drives the active pane until the user detaches (`Ctrl-O` → 切替) or every
    // pane has closed (→ 在席). Switching tabs and adding panes now happen in 切替,
    // not here. `new_pane` distinguishes the two ways in: `false` re-attaches the
    // session's active pane (spawning the first when it is fresh); `true` adds a
    // new pane of the requested kind and drives it (the 在席 action surface's
    // `terminal` / `agent`). The attached session is declared to the monitor (so
    // it is never flagged as waiting) and cleared again on the way out.
    let terminal_root = workspace.path.clone();
    let mut open_terminal = |home: &mut HomeState,
                             dir: &Path,
                             run_agent: bool,
                             new_pane: bool|
     -> Result<PaneExit> {
        // Build the agent command for this worktree on demand: when it already
        // has a Claude conversation, launch with `--continue` so `:agent` resumes
        // where it left off; otherwise it starts fresh. The pool only sends it on
        // a fresh agent-pane spawn (re-attaching / terminal panes never use it).
        // It is built unconditionally (not just for `run_agent`) so a later
        // `Ctrl-O a` can spawn an agent pane too.
        let resume = agent.has_resumable_session(dir);
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
        // Deliver a prompt queued for this session (via MCP `session_prompt`) only
        // when this attach will *freshly spawn* its agent pane — `add_pane` always
        // spawns; `enter` spawns only when no pane is live yet. Taking it makes the
        // prompt one-shot; if no fresh agent spawn happens it stays queued for the
        // next launch. The agent then opens already working on that prompt.
        let fresh_agent_spawn = run_agent && (new_pane || !pool.has_live_pane(dir));
        let queued_prompt = if fresh_agent_spawn {
            crate::infrastructure::agent_prompt_store::take(dir)
        } else {
            None
        };
        // The command for this fresh spawn carries the queued prompt; the command
        // reused for later `Ctrl-O a` agent tabs never re-sends that one-shot
        // prompt, so only the first launch receives it.
        let spawn_command = agent.launch_command(&agent_wiring, resume, queued_prompt.as_deref());
        let plain_command = match queued_prompt {
            Some(_) => agent.launch_command(&agent_wiring, resume, None),
            None => spawn_command.clone(),
        };
        let initial = Some(spawn_command.as_str());
        let later_initial = Some(plain_command.as_str());
        // Capture every failure of this launch — the initial spawn (`add_pane`
        // / `enter`) and anything during the pane loop — in one `result`, so a
        // launch that never gets a live pane is cleaned up and logged just like a
        // mid-session failure instead of returning early past the cleanup and the
        // error log below.
        let result = (|| -> Result<PaneExit> {
            // Ready the pane to drive: add a fresh one (在席's `terminal` /
            // `agent`) or re-attach the session's active pane (spawning the first
            // when the session is new).
            if new_pane {
                let kind = if run_agent {
                    terminal_tabs::PaneKind::Agent
                } else {
                    terminal_tabs::PaneKind::Terminal
                };
                pool.add_pane(term, dir, kind, initial, &label)?;
            } else {
                pool.enter(term, dir, run_agent, initial, &label)?;
            }
            handle.set_attached(Some(dir.to_path_buf()));
            loop {
                // Publish the tab strip for this session before driving the pane,
                // so it reflects any add / close / switch from the last step.
                let (labels, active_tab) = pool.tabs(dir);
                home.set_terminal_tabs(labels, active_tab);
                let pty = match pool.active_pty(dir) {
                    Some(pty) => pty,
                    // No live pane (every one exited): drop back to 在席.
                    None => return Ok(PaneExit::Closed),
                };
                match terminal_pane::run(term, home, pty, &handle)? {
                    // `Ctrl-O`: zoom out to 切替, leaving every pane alive.
                    terminal_pane::PaneStep::Detach => return Ok(PaneExit::ToSwitch),
                    // `Ctrl-N` / `Ctrl-P`: move the active tab and loop, so the
                    // next iteration drives the newly active pane (and republishes
                    // the tab strip above it) without leaving 没入.
                    terminal_pane::PaneStep::NextTab => pool.nav(dir, terminal_tabs::TabNav::Next),
                    terminal_pane::PaneStep::PrevTab => pool.nav(dir, terminal_tabs::TabNav::Prev),
                    // `Ctrl-T` / `Ctrl-G`: add a terminal / agent tab and loop, so
                    // the next iteration drives the freshly added (now active) pane
                    // and republishes the tab strip — without leaving 没入.
                    terminal_pane::PaneStep::NewTerminalTab => {
                        pool.add_pane(
                            term,
                            dir,
                            terminal_tabs::PaneKind::Terminal,
                            later_initial,
                            &label,
                        )?;
                    }
                    terminal_pane::PaneStep::NewAgentTab => {
                        pool.add_pane(
                            term,
                            dir,
                            terminal_tabs::PaneKind::Agent,
                            later_initial,
                            &label,
                        )?;
                    }
                    // `Ctrl-W`: close the active tab. Same as a shell that exited —
                    // keep driving when a pane remains, else fall to 在席.
                    terminal_pane::PaneStep::CloseTab => {
                        if !pool.close_active(dir, &label) {
                            return Ok(PaneExit::Closed);
                        }
                    }
                    // The active pane's shell exited: drop it; keep driving when
                    // a pane remains, else fall to 在席.
                    terminal_pane::PaneStep::Closed => {
                        if !pool.close_active(dir, &label) {
                            return Ok(PaneExit::Closed);
                        }
                    }
                }
            }
        })();
        // Leaving the pane (Ctrl-O → 切替, every pane closing, or an error) means
        // nothing is attached any more; the shells themselves stay alive in the
        // pool. Clear the whole surface (snapshot + tab strip) here, where the
        // pane yields control, rather than relying on the event loop's next frame
        // to mop up the stale screen snapshot — so the cleanup holds no matter
        // when control changes hands.
        handle.set_attached(None);
        home.clear_terminal_surface();
        // The user may have committed / pushed / merged while in the pane, so
        // re-sync the worktree statuses now that they have left it — keeping the
        // cursor where it is. Best-effort: a sync failure just leaves the
        // last-known statuses in place.
        if let Some(sessions) = reload_sessions(&terminal_root) {
            home.refresh_sessions(sessions);
        }
        // The event loop shows a launch / pane failure on screen; also persist it
        // (with its full cause chain) so a session that failed to start stays
        // inspectable in the error log after the fact.
        if let Err(e) = &result {
            crate::infrastructure::error_log::ErrorLog::record(&format!(
                "{} session in {} failed: {e:#}",
                if run_agent { "agent" } else { "terminal" },
                dir.display()
            ));
        }
        result
    };

    // Snapshot the selected session's live terminal for the sidebar's right-pane
    // preview (the tab-like view), or `None` when it has no running shell/agent.
    let mut preview =
        |dir: &Path| -> Option<crate::presentation::tui::home::terminal_view::TerminalView> {
            pool.borrow_mut().snapshot(term, dir)
        };

    // Read (and optionally navigate) a session's tabs from 切替: `←`/`→` pass a
    // `TabNav` to move the active tab, and the loop reads the strip (`None`) each
    // frame to draw it above the preview. Both go through the same pool the pane
    // driver uses, so a tab moved here is the one re-attaching reveals.
    let mut tab_op = |dir: &Path, nav: Option<terminal_tabs::TabNav>| -> (Vec<String>, usize) {
        let mut pool = pool.borrow_mut();
        if let Some(nav) = nav {
            pool.nav(dir, nav);
        }
        pool.tabs(dir)
    };

    // Close the highlighted session's active tab (pane) from 切替 (`x`): kill its
    // shell through the same pool the pane driver uses, so the next frame's tab
    // read reflects the removal. The session's display label re-registers its
    // remaining panes with the monitor under the right name.
    let mut close_tab = |home: &mut HomeState, dir: &Path| {
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
        pool.borrow_mut().close_active(dir, &label);
    };

    // Opening `config` hands off to the settings screen in its workspace scope,
    // editing only this workspace's local overrides
    // (`<workspace>/.usagi/settings.json`); the global settings are changed from
    // the CLI or welcome menu instead. Quitting there (Ctrl+C) quits the app,
    // reported back as `true` so the event loop propagates the quit; `Back`
    // returns `false`.
    let config_root = workspace.path.clone();
    let mut open_config = |t: &Term| -> Result<Option<event::ConfigReload>> {
        match crate::presentation::tui::config::run_in(t, Some(config_root.clone()))? {
            // Back to home: re-read the (possibly changed) Session Action UI and
            // local LLM availability so the 在席 surface and `ai` command reflect
            // the edit without reopening the home screen.
            crate::presentation::tui::config::Outcome::Back => {
                let settings = effective_settings(&config_root);
                Ok(Some(event::ConfigReload {
                    session_action_ui: settings.session_action_ui,
                    ai_available: local_llm_available(&settings),
                }))
            }
            crate::presentation::tui::config::Outcome::Quit => Ok(None),
        }
    };

    let mut wiring = event::Wiring {
        workspace_root: &workspace.path,
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename_display,
        dispatch_remove: &mut dispatch_remove,
        evict_pool: &mut evict_pool,
        existing_branches: &mut existing_branches,
        open_terminal: &mut open_terminal,
        open_config: &mut open_config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close_tab,
    };
    let outcome = event::event_loop(
        term,
        &mut reader,
        state,
        &monitor,
        &update,
        &tasks,
        &mut wiring,
    );

    // The loop has exited (quit / back), so wait for any background create /
    // remove still running before returning — otherwise the process could tear
    // down the worker mid-`worktree add` / `remove` and leave a half-written
    // worktree or `state.json`. Workers that already finished join instantly;
    // at most this waits out the git work in flight (serialised by the op-lock).
    // Their completions go undrained, which is fine: nothing renders after exit
    // and the pool (its shells) is about to be dropped anyway.
    for worker in workers.borrow_mut().drain(..) {
        let _ = worker.join();
    }

    outcome
}

/// Create a session on a worker thread: run the git / filesystem work and build
/// the [`Completion`](tasks::Completion) the event loop applies (the success or
/// failure line, and the refreshed sessions read back with each worktree's git
/// status). The `bool` is whether it succeeded, for the task row's mark.
fn run_create(root: &Path, name: &str) -> (bool, tasks::Completion) {
    match crate::usecase::session::create(root, name) {
        Ok(created) => (
            true,
            tasks::Completion {
                line: LogLine::output(format!(
                    "Created session \"{}\" ({} worktree(s)) 🐰",
                    created.name,
                    created.worktrees.len()
                )),
                sessions: reload_sessions(root),
                evict: None,
            },
        ),
        Err(e) => {
            crate::infrastructure::error_log::ErrorLog::record(&format!(
                "session create \"{name}\" failed: {e:#}"
            ));
            (
                false,
                tasks::Completion {
                    line: LogLine::error(format!("session failed: {e}")),
                    sessions: None,
                    evict: None,
                },
            )
        }
    }
}

/// Remove a session on a worker thread: run the git / filesystem work and build
/// the [`Completion`](tasks::Completion) the event loop applies. A successful
/// removal carries the refreshed sessions and the session root whose pooled
/// shell to evict; a session with uncommitted changes (without `--force`) only
/// logs how to discard them. The `bool` is whether it removed the session.
fn run_remove(
    root: &Path,
    name: &str,
    force: bool,
    agent: &dyn crate::domain::agent::Agent,
) -> (bool, tasks::Completion) {
    match crate::usecase::session::remove(root, name, force, agent) {
        Ok(outcome) if outcome.removed => (
            true,
            tasks::Completion {
                line: LogLine::output(format!("Removed session \"{name}\" 🧹")),
                sessions: reload_sessions(root),
                evict: Some(root.join(".usagi").join("sessions").join(name)),
            },
        ),
        Ok(outcome) => {
            let paths = outcome
                .dirty
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            (
                false,
                tasks::Completion {
                    line: LogLine::error(format!(
                        "session \"{name}\" has uncommitted changes ({paths}). \
                         Use \"session remove {name} --force\" to discard."
                    )),
                    sessions: None,
                    evict: None,
                },
            )
        }
        Err(e) => {
            crate::infrastructure::error_log::ErrorLog::record(&format!(
                "session remove \"{name}\" failed: {e:#}"
            ));
            (
                false,
                tasks::Completion {
                    line: LogLine::error(format!("session remove failed: {e}")),
                    sessions: None,
                    evict: None,
                },
            )
        }
    }
}

/// The effective settings (project-local overrides on top of the global
/// default) for the workspace at `root`. Read at startup and again whenever the
/// config screen closes, so an edited setting takes effect without reopening the
/// home screen. Any failure to read settings falls back to the defaults.
fn effective_settings(root: &Path) -> crate::domain::settings::Settings {
    crate::infrastructure::storage::Storage::open_default()
        .and_then(|storage| crate::usecase::settings::effective(&storage, root))
        .unwrap_or_default()
}

/// Whether the local LLM is usable right now: enabled in settings and its model
/// already pulled into the `ollama` runtime. Gates the `ai` command in the 在席
/// (Focus) menu so it appears only when running it would actually work. The
/// model probe is an `ollama show`, skipped entirely when the feature is off.
fn local_llm_available(settings: &crate::domain::settings::Settings) -> bool {
    settings.local_llm.enabled
        && crate::usecase::local_llm::model_present(
            &crate::usecase::doctor::SystemRunner,
            &settings.local_llm.model,
        )
}
