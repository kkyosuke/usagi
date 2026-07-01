//! Home screen (画面 #5, workspace view).
//!
//! Opened after a workspace is chosen on the project selection screen. Shows
//! the workspace's worktrees, loaded from its `<workspace>/.usagi/state.json`,
//! and lets the user pick one. Acting on a worktree is a placeholder for now —
//! the per-worktree session screen is not implemented yet — so selecting one
//! shows a "coming soon" notice.

pub mod command;
pub mod event;
pub mod oneshot;
pub mod pane_input;
pub mod sessions_refresh;
pub mod state;
pub mod tasks;
pub mod terminal;
pub mod ui;
pub mod update;

#[cfg(test)]
mod e2e_tests;

use std::path::{Path, PathBuf};

use anyhow::Result;
use console::Term;

use crate::domain::workspace::Workspace;
use crate::domain::workspace_state::SessionRecord;
use crate::presentation::tui::io::term_reader::TermKeyReader;

pub use event::Outcome;

use state::{
    HomeState, LogLine, PaneExit, ResumeLevel, SessionOutcome, SessionReorder, SurfaceOwner,
    ROOT_NAME,
};

/// Refresh the workspace's session state from git (best-effort) and return the
/// sessions to show. `sync` rewrites each session worktree's status; for a
/// non-git root it fails harmlessly, so we fall back to the saved sessions
/// (via the usecase, which owns the store access).
fn reload_sessions(root: &Path) -> Option<Vec<SessionRecord>> {
    if let Ok(state) = crate::usecase::workspace_state::sync(root) {
        return Some(state.sessions);
    }
    crate::usecase::workspace_state::recorded_sessions(root)
}

/// Track a freshly spawned session-worker handle, first dropping the handles of
/// workers that have already finished. The set is only fully drained when the
/// screen exits (joining any in-flight git work), so without this reap a long
/// session that creates / removes / detaches many times would accumulate finished
/// `JoinHandle`s without bound. Reaping keeps the Vec sized to roughly the workers
/// actually in flight.
fn track_worker(
    workers: &std::cell::RefCell<Vec<std::thread::JoinHandle<()>>>,
    handle: std::thread::JoinHandle<()>,
) {
    let mut workers = workers.borrow_mut();
    workers.retain(|h| !h.is_finished());
    workers.push(handle);
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

/// The workspace data [`run`] needs at startup, loaded from disk without a
/// `Term`: the recorded sessions (and any load-error notice), task issues,
/// effective settings, and command history.
///
/// Building this is part of opening a workspace (several disk reads). [`preload`]
/// computes it with no terminal or thread state, so the caller can run it on a
/// background thread *while the open→home mascot animation plays* and hand the
/// result to [`run`] — the home screen then paints the instant it is shown instead
/// of blocking the first frame on these reads. The agent-CLI PATH probe is *not*
/// here: it shells out to each CLI (`--version`), which can outlast the animation,
/// so [`run`] runs it on its own background thread and swaps the result in once it
/// lands (like the local-LLM probe), keeping the first paint off the subprocesses.
pub struct Preload {
    sessions: Vec<SessionRecord>,
    root_note: Option<String>,
    notice: Option<String>,
    issues: Vec<crate::domain::issue::Issue>,
    settings: crate::domain::settings::Settings,
    history: Vec<String>,
}

/// Loads the [`Preload`] for a workspace. Pure disk / PATH work with no `Term`,
/// so it is safe to run on a background thread behind the open→home animation. A
/// read failure for any part is non-fatal — that part falls back to empty (the
/// session load surfaces its error as the screen's notice instead).
pub fn preload(workspace: &Workspace) -> Preload {
    // The recorded sessions come from `state.json` (no git): the screen opens from
    // these immediately, then re-syncs the worktree statuses from git on a
    // background thread (spawned in `run`) and swaps the refreshed sessions in when
    // they land. Syncing here would block the first paint on git for as long as the
    // workspace has worktrees to inspect. A load error surfaces as a notice; a
    // non-git root or a read failure just leaves these saved statuses.
    let (sessions, notice) =
        crate::usecase::workspace_state::recorded_sessions_for_display(&workspace.path);
    // The `⌂ root` row's memo, loaded from the same recorded state (no git) so the
    // sidebar marker and the 切替 preview show it on the first paint, mirroring how
    // a session's note is loaded with the session.
    let root_note = crate::usecase::workspace_state::recorded_root_note(&workspace.path);
    // Task issues back the `issue` command (list / graph / show); none on failure.
    let issues = crate::infrastructure::issue_store::IssueStore::new(&workspace.path)
        .scan()
        .unwrap_or_default();
    // The effective settings (project-local overrides on top of the global
    // default), reused for every setting the screen derives — the 在席 action
    // surface, the sidebar's initial state, the local-LLM probe, the agent CLI /
    // wiring, and notifications. Re-read whenever the config screen closes (see
    // `open_config`) so a change takes effect without reopening this screen.
    let settings = effective_settings(&workspace.path);
    // Past commands so `history` and `↑`/`↓` recall span sessions; empty on failure.
    let history = crate::usecase::history::load(&workspace.path)
        .map(|entries| entries.into_iter().map(|e| e.command).collect())
        .unwrap_or_default();
    Preload {
        sessions,
        root_note,
        notice,
        issues,
        settings,
        history,
    }
}

/// Runs the home screen for `workspaces` on the given terminal until the user
/// goes back or quits, wiring the already-loaded [`Preload`] and the real
/// terminal to the testable event loop in [`event`]. The caller loads the
/// `Preload` (see [`preload`]) — on the Open screen, off-thread behind the mascot
/// animation — so this does no blocking IO before the first paint. Each command
/// the user runs is appended to the workspace's `history.json` (best-effort).
/// Assumes the alternate screen is already active (it is owned by the
/// orchestrator).
///
/// `workspaces[0]` is the *primary* workspace the `Preload` belongs to — the one
/// the live re-sync, `session` commands, and root row act on. Any further entries
/// are 統合(unite) mode: their sessions are loaded here (the animation has already
/// played) and stacked below the primary as display groups.
pub fn run(term: &Term, workspaces: &[Workspace], preload: Preload) -> Result<Outcome> {
    let workspace = &workspaces[0];
    // All the disk / PATH reads were done by `preload` (off-thread, behind the
    // animation), so opening the screen here is just wiring that data into the
    // state — no blocking IO before the first paint.
    let Preload {
        sessions,
        root_note,
        notice,
        issues,
        settings,
        history,
    } = preload;

    let mut state = HomeState::new(workspace.name.clone(), Vec::new(), notice);
    // The root row (`⌂ root`) operates in the workspace root; record its path so
    // the 切替 preview can recognise the root's live embedded session (keyed by
    // this path) and show its terminal, mirroring how worktree rows are matched.
    state.set_root_path(workspace.path.clone());
    // Persist on-screen operation failures to the daily error log: the screen's
    // single error sink (`HomeState::log_error` and the failure lines applied from
    // background tasks / session outcomes) writes through this. Tests leave the
    // no-op default and record nothing.
    state.set_logger(Box::new(crate::infrastructure::error_log::FileLogger));
    state.restore_sessions(sessions);
    state.restore_root_note(root_note);
    // 統合(unite) mode: load the other selected workspaces' recorded sessions and
    // stack them below the primary as display groups. The mascot animation has
    // already played, so this synchronous read does not delay the first paint.
    if workspaces.len() > 1 {
        let extras: Vec<state::GroupSource> = workspaces[1..]
            .iter()
            .map(|w| {
                let (sessions, _) =
                    crate::usecase::workspace_state::recorded_sessions_for_display(&w.path);
                let root_note = crate::usecase::workspace_state::recorded_root_note(&w.path);
                state::GroupSource {
                    name: w.name.clone(),
                    root_path: w.path.clone(),
                    root_note,
                    sessions,
                }
            })
            .collect();
        state.set_extra_groups(extras);
    }
    state.set_issues(issues);
    // Which right-pane action surface 在席 (Focus) presents — a pickable menu or a
    // typed prompt — and the state the left sidebar opens in (full width or the
    // collapsed rail; `Ctrl-B` toggles it from there).
    state.set_session_action_ui(settings.session_action_ui);
    state.set_sidebar(settings.sidebar);
    // How the embedded terminal (没入) reserves its navigation keys — a `Ctrl-O`
    // prefix or single `Alt`-chords — so the rest reach the shell / agent.
    state.set_key_scheme(settings.key_scheme);
    // Whether the sidebar mascot reacts to interaction (a blink in 切替 / 在席, the
    // 没入 paw); off keeps it a still resting image.
    state.set_mascot_animation_enabled(settings.mascot_animation_enabled);
    // The configured default agent (its display name labels 在席's `agent` row and
    // a bare `agent` launches it). The agents installed on this machine fill in
    // shortly after via the background probe spawned below (state opens with none).
    state.set_default_agent(settings.agent_cli);
    // The screen opens in 切替 (Switch) — the base mode (see `HomeState::new`) —
    // so selecting a project lands on the session list the mascot animation glides
    // into; no explicit mode switch is needed here.

    state.restore_history(history);

    let mut reader = TermKeyReader::new(term.clone());
    // Persisting a command is best-effort; a write failure must not break the
    // screen, so the error is intentionally dropped (cf. `hop`'s notification).
    let history_root = workspace.path.clone();
    let mut persist = move |command: &str| {
        let _ = crate::usecase::history::append(&history_root, command);
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
    let create_lock = op_lock.clone();
    let create_workers = workers.clone();
    // `root` is the workspace the cursor's group points at (the primary, or an
    // extra 統合/unite workspace), so the session lands where the user is pointing.
    let mut dispatch_create = move |root: &Path, name: &str, interaction_epoch: u64| {
        let id = create_tasks.begin(tasks::TaskKind::CreateSession, name);
        let handle = create_tasks.clone();
        let root = root.to_path_buf();
        let name = name.to_string();
        let lock = create_lock.clone();
        let worker = std::thread::spawn(move || {
            complete_or_record_panic(&handle, id, tasks::TaskKind::CreateSession, &name, || {
                let _guard = lock_session_ops(&lock);
                run_create(&root, &name, interaction_epoch)
            });
        });
        track_worker(&create_workers, worker);
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
    let rename_lock = op_lock.clone();
    let mut rename_display = |root: &Path, name: &str, label: &str| {
        let _guard = lock_session_ops(&rename_lock);
        match crate::usecase::session::set_display_name(root, name, label) {
            // The usecase persists the raw override; the label shown falls back to
            // the session name when the override was cleared (presentation's call).
            Ok(stored) => SessionOutcome {
                line: LogLine::output(format!(
                    "Renamed \"{name}\" to \"{}\" 🏷",
                    stored.as_deref().unwrap_or(name)
                )),
                sessions: reload_sessions(root),
                select: Some(name.to_string()),
                root_note: None,
            },
            Err(e) => SessionOutcome {
                line: LogLine::error(format!("rename failed: {e}")),
                sessions: None,
                select: None,
                root_note: None,
            },
        }
    };

    // Saving a note persists it to state.json and re-reads the sessions so the
    // editor's next open reflects it. The `⌂ root` row carries its own note (kept
    // on the workspace state, not a session), so a `name` of [`ROOT_NAME`] routes
    // to `set_root_note` and carries the stored value back as `root_note`; every
    // other name edits the named session's note. Like `rename_display` this stays
    // synchronous (no git work) but still load-modify-saves `state.json`, so it
    // takes the same op-lock to serialise against the background create / remove
    // workers. `select` keeps the cursor on the edited session after the rebuild.
    let note_lock = op_lock.clone();
    let mut set_note = |root: &Path, name: &str, note: &str| {
        let _guard = lock_session_ops(&note_lock);
        let is_root = name == ROOT_NAME;
        let result = if is_root {
            crate::usecase::session::set_root_note(root, note)
        } else {
            crate::usecase::session::set_note(root, name, note)
        };
        match result {
            Ok(stored) => SessionOutcome {
                line: LogLine::output(match stored {
                    Some(_) => format!("Saved note for \"{name}\" 📝"),
                    None => format!("Cleared note for \"{name}\" 📝"),
                }),
                sessions: reload_sessions(root),
                // The root row is not selectable by session name; the session path
                // keeps the cursor on the session it edited.
                select: (!is_root).then(|| name.to_string()),
                // Only the root-note save reports a new root note for the screen to
                // pick up; a session note leaves the in-memory root note untouched.
                root_note: is_root.then_some(stored),
            },
            Err(e) => SessionOutcome {
                line: LogLine::error(format!("note failed: {e}")),
                sessions: None,
                select: None,
                root_note: None,
            },
        }
    };

    // Reordering a session (`K` / `J` in 切替) swaps it with its neighbour in
    // state.json and re-reads the sessions so the pane reflects the new order.
    // Like rename / note it stays synchronous (no git work) but still
    // load-modify-saves state.json, so it takes the same op-lock to serialise
    // against the background create / remove workers.
    let reorder_root = workspace.path.clone();
    let reorder_lock = op_lock.clone();
    let mut reorder_session = |name: &str, up: bool| {
        let _guard = lock_session_ops(&reorder_lock);
        match crate::usecase::session::reorder(&reorder_root, name, up) {
            // A successful move re-reads the (now reordered) sessions; if the
            // re-read somehow yields nothing, treat it as no change rather than
            // blanking the pane.
            Ok(true) => match reload_sessions(&reorder_root) {
                Some(sessions) => SessionReorder::Moved(sessions),
                None => SessionReorder::Stationary,
            },
            // An edge move (first up / last down) changed nothing.
            Ok(false) => SessionReorder::Stationary,
            Err(e) => SessionReorder::Failed(LogLine::error(format!("reorder failed: {e}"))),
        }
    };

    // Whether the 在席 (Focus) menu offers the `ai` command: only when the local
    // LLM is enabled and its model is pulled, so it appears only when running it
    // would actually work. The probe is an `ollama show`, which can block on a
    // cold / wedged `ollama` server, so it runs on a background thread rather than
    // delaying the first paint: the menu omits `ai` until the probe lands, and the
    // event loop flips it on when it does. Re-probed when the config screen closes
    // (see `open_config`).
    let ai_available = oneshot::OneShot::new();
    {
        let handle = ai_available.clone();
        let settings = settings.clone();
        std::thread::spawn(move || {
            handle.set(local_llm_available(&settings));
        });
    }

    // The agents installed on this machine (which 在席's agent picker offers as
    // alternatives to the configured default). Probing them shells out to each
    // candidate CLI with `--version`, one after another, which can take longer than
    // the open→home animation — so, like the local-LLM probe above, it runs on a
    // background thread instead of in `preload`: the picker simply offers no
    // alternatives until the probe lands, and the event loop swaps them in when it
    // does. Keeping it off `preload` is what stops the home screen from stalling
    // after the mascot lands while the join waits on the subprocesses.
    let installed_agents = oneshot::OneShot::new();
    {
        let handle = installed_agents.clone();
        std::thread::spawn(move || {
            handle.set(crate::usecase::agent::available_clis(
                &crate::usecase::doctor::SystemRunner,
            ));
        });
    }

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
    // single source of the launch command built per worktree below. Session
    // removal cleans up after this default agent; a launch may instead use the CLI
    // the user picked in 在席 (resolved per-launch in `open_terminal` below).
    let agent = crate::infrastructure::agent::agent_for(settings.agent_cli);
    // The configured default CLI, the fallback when a launch carries no explicit
    // agent choice (a bare `agent`, the menu's `a` shortcut / default row).
    let default_cli = settings.agent_cli;

    // Whether each session's open panes are persisted (so they restore on the next
    // startup). Copied out so the pane driver can read it without holding `settings`.
    let restore_panes_enabled = settings.restore_panes_enabled;

    // usagi's wiring policy (resolved usagi binary + local LLM model) the agent
    // renders into its own invocation; built once and reused for every launch.
    let agent_wiring = settings.agent_wiring(&usagi_bin);

    // Whether to surface desktop notifications when a background session starts
    // waiting for input or finishes. Opt-out: on unless the user disabled it.
    let notifications_enabled = settings.notifications_enabled;

    // How much scrollback each embedded pane keeps. Paid once per live pane, so a
    // smaller cap is the main lever on the screen's memory when many sessions and
    // panes are open. Already clamped by `Settings::sanitized` on load.
    let scrollback_lines = settings.terminal_scrollback_lines;

    // The live shells embedded in the right pane, one per worktree, kept alive
    // across session switches and for as long as this screen is open. Dropped on
    // return, which kills any shell still running. The pool also watches every
    // shell's bell/phase and flags / notifies the ones waiting or finished.
    //
    // Wrapped in a `RefCell` so the pane driver (`open_terminal`), the sidebar
    // preview (`preview`), and `remove_session` (which evicts a removed session's
    // shell) can all reach it: their borrows never overlap in time (the event
    // loop calls one at a time).
    let pool = std::cell::RefCell::new(terminal::pool::TerminalPool::new(
        notifications_enabled,
        scrollback_lines,
    ));
    let monitor = pool.borrow().monitor();

    // Restore each session's panes from the last run, in the background (nothing is
    // attached yet): an agent relaunches resuming its conversation, a terminal
    // reopens a fresh shell. The watcher then tracks them so the sidebar badges
    // move without the user attaching. Gated by the setting; a fresh workspace or a
    // disabled setting simply starts with no panes.
    if restore_panes_enabled {
        restore_open_panes(term, &state, &pool, &agent_wiring, default_cli);
        // Restore where the user was at the last quit — the cursor on a session
        // (切替), focused (在席), or armed to auto-attach (没入). Done after the panes
        // are back, so a 没入 target's pane is live for the event loop's first-pass
        // attach. Best-effort: a missing snapshot or a since-removed session simply
        // opens in the default 切替.
        if let Some(focus) = crate::infrastructure::resume_focus_store::load(&workspace.path) {
            use crate::infrastructure::resume_focus_store::StoredEngagement;
            let level = match focus.engagement {
                StoredEngagement::Switch => ResumeLevel::Switch,
                StoredEngagement::Focus => ResumeLevel::Focus,
                StoredEngagement::Attached => ResumeLevel::Attached,
            };
            state.restore_focus(&focus.session, level);
        }
    }

    // Removing a session deletes its worktrees/branches and forgets it, on a
    // background thread like creation so the screen never freezes. A session with
    // uncommitted changes is left untouched unless `--force`. The git work runs
    // under the op-lock; the result (and, on success, the pool path whose shell
    // to evict) is stored for the event loop to drain.
    let remove_tasks = tasks.clone();
    let remove_lock = op_lock.clone();
    let remove_agent = agent.clone();
    let remove_workers = workers.clone();
    // `root` is the workspace the targeted session lives in (the cursor's group in
    // 統合/unite mode), resolved by the handler before dispatch.
    let mut dispatch_remove = move |root: &Path,
                                    name: &str,
                                    force: bool,
                                    focus: Option<tasks::AutoFocus>| {
        let id = remove_tasks.begin(tasks::TaskKind::RemoveSession, name);
        let handle = remove_tasks.clone();
        let root = root.to_path_buf();
        let name = name.to_string();
        let lock = remove_lock.clone();
        let agent = remove_agent.clone();
        let worker = std::thread::spawn(move || {
            complete_or_record_panic(&handle, id, tasks::TaskKind::RemoveSession, &name, || {
                let _guard = lock_session_ops(&lock);
                run_remove(&root, &name, force, agent.as_ref(), focus)
            });
        });
        track_worker(&remove_workers, worker);
    };

    // Evict a removed session's still-running shell from the pool so a session
    // later recreated at the same path starts fresh instead of re-attaching to
    // this run's agent and its history. Run by the event loop when it drains a
    // finished removal — on this thread, since the pool is not `Send` and so
    // cannot be touched from the worker thread.
    let mut evict_pool = |session_root: &Path| {
        // Also drop the removed worktrees' persisted pane snapshots, so a session
        // recreated at the same path starts fresh instead of restoring this run's
        // panes on the next startup (mirrors why the pool entry is evicted).
        for dir in pool.borrow_mut().remove_under(session_root) {
            crate::infrastructure::open_panes_store::clear(&dir);
        }
    };

    // Check the project's git remote for a newer release than this build, on a
    // background thread so a slow or unreachable network never delays the screen.
    // The result is written to the handle the event loop reads each redraw; when
    // a newer version is published the sidebar mascot speaks an "update available"
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

    // Leaving an embedded pane re-syncs the worktree statuses from git (a commit
    // / push / merge may have happened inside it). That sync is slow — a `git
    // status` per worktree, plus the cross-process state lock — exactly when
    // several sessions are running agents, so it runs on a background thread and
    // writes the refreshed list here; the event loop applies it on a later frame
    // instead of the detach freezing until git returns.
    let sessions_refresh = sessions_refresh::SessionsRefreshHandle::new();
    // Re-sync the same statuses once on entry, through the same handle: the screen
    // opened immediately from the saved `state.json` (above) without waiting on
    // git, so kick the status sync onto a background thread and let the event loop
    // swap in the refreshed list when it lands. `workspace_state::sync` serialises
    // its own `state.json` write through the store's cross-process lock, so it is
    // safe to run alongside the background session create / remove workers. A
    // non-git root or a sync failure leaves the saved statuses in place.
    {
        let handle = sessions_refresh.clone();
        let root = workspace.path.clone();
        std::thread::spawn(move || {
            if let Ok(state) = crate::usecase::workspace_state::sync(&root) {
                handle.set(state.sessions);
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
    let mut open_terminal =
        |home: &mut HomeState, dir: &Path, run_agent: bool, new_pane: bool| -> Result<PaneExit> {
            // Resolve which agent CLI this launch drives: the user's 在席 choice (menu
            // picker / `agent <name>`), consumed here so it applies once, falling back
            // to the configured default. `take` clears it whether or not a fresh agent
            // spawn follows, so a stale choice never leaks into a later launch.
            let cli = home.take_agent_choice().unwrap_or(default_cli);
            let agent = crate::infrastructure::agent::agent_for(cli);
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
            // A session holds at most one agent: a request to add an agent pane
            // (在席's `agent`, or `Ctrl-G` routed through `new_pane`) when one already
            // exists reuses it — activating its tab below — rather than spawning a
            // second. This also keeps the queued prompt unconsumed (no fresh spawn).
            let reuse_agent = run_agent && new_pane && pool.has_agent_pane(dir);
            // Deliver a prompt queued for this session (via MCP `session_prompt`) only
            // when this attach will *freshly spawn* its agent pane — `add_pane` always
            // spawns; `enter` spawns only when no pane is live yet; reusing an existing
            // agent never spawns. Taking it makes the prompt one-shot; if no fresh agent
            // spawn happens it stays queued for the next launch. The agent then opens
            // already working on that prompt.
            let fresh_agent_spawn =
                run_agent && !reuse_agent && (new_pane || !pool.has_live_pane(dir));
            let queued_prompt = if fresh_agent_spawn {
                crate::infrastructure::agent_prompt_store::take(dir)
            } else {
                None
            };
            // The command for this fresh spawn carries the queued prompt; the command
            // reused for later `Ctrl-O a` agent tabs never re-sends that one-shot
            // prompt, so only the first launch receives it.
            let spawn_command =
                agent.launch_command(&agent_wiring, resume, queued_prompt.as_deref());
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
                // Ready the pane to drive: reuse the lone agent when a second was
                // requested, add a fresh pane (在席's `terminal` / `agent`), or
                // re-attach the session's active pane (spawning the first when the
                // session is new).
                if reuse_agent {
                    pool.activate_agent(dir);
                } else if new_pane {
                    let kind = if run_agent {
                        terminal::tabs::PaneKind::Agent
                    } else {
                        terminal::tabs::PaneKind::Terminal
                    };
                    pool.add_pane(term, dir, kind, initial, cli, &label)?;
                } else {
                    pool.enter(term, dir, run_agent, initial, cli, &label)?;
                }
                handle.set_attached(Some(dir.to_path_buf()));
                loop {
                    // Publish the tab strip for this session before driving the pane,
                    // so it reflects any add / close / switch from the last step.
                    let (labels, active_tab) = pool.tabs(dir);
                    home.surface_writer(SurfaceOwner::Attached)
                        .set_tabs(labels, active_tab);
                    let pty = match pool.active_pty(dir) {
                        Some(pty) => pty,
                        // No live pane (every one exited): drop back to 在席.
                        None => return Ok(PaneExit::Closed),
                    };
                    match terminal::pane::run(term, home, pty, &handle)? {
                        // `Ctrl-O`: zoom out to 切替, leaving every pane alive.
                        terminal::pane::PaneStep::Detach => return Ok(PaneExit::ToSwitch),
                        // `Ctrl-E`: leave the pane to open the note editor over it;
                        // the event loop re-attaches when the editor closes.
                        terminal::pane::PaneStep::OpenNote => return Ok(PaneExit::OpenNote),
                        // `Ctrl-N` / `Ctrl-P`: move the active tab and loop, so the
                        // next iteration drives the newly active pane (and republishes
                        // the tab strip above it) without leaving 没入.
                        terminal::pane::PaneStep::NextTab => {
                            let _ = pool.nav(dir, terminal::tabs::TabNav::Next);
                        }
                        terminal::pane::PaneStep::PrevTab => {
                            let _ = pool.nav(dir, terminal::tabs::TabNav::Prev);
                        }
                        // `Ctrl+Shift+N` / `Ctrl+Shift+P`: reorder the active tab
                        // in place and keep driving that same pane at its new slot.
                        terminal::pane::PaneStep::SwapTabRight => {
                            let _ = pool.swap_active(dir, terminal::tabs::TabSwap::Right);
                        }
                        terminal::pane::PaneStep::SwapTabLeft => {
                            let _ = pool.swap_active(dir, terminal::tabs::TabSwap::Left);
                        }
                        // A click on a tab chip: jump straight to that pane and loop,
                        // driving it (and republishing the strip) without leaving 没入.
                        terminal::pane::PaneStep::ToTab(i) => {
                            let _ = pool.nav(dir, terminal::tabs::TabNav::To(i));
                        }
                        // Dragging one tab chip onto another reorders the pane
                        // list; the moved pane stays active at its new slot.
                        terminal::pane::PaneStep::MoveTab { from, to } => {
                            let _ = pool.move_tab(dir, from, to);
                        }
                        // `Ctrl-T`: zoom out to 在席 (Focus) so the user picks the next
                        // action from the session's menu, leaving every pane alive in
                        // the pool (like `Ctrl-O`, but one level shallower).
                        terminal::pane::PaneStep::ToFocus => return Ok(PaneExit::ToFocus),
                        // `Ctrl-G`: a session holds at most one agent — jump to the
                        // existing agent tab when present, else add one (then loop, so
                        // the next iteration drives it and republishes the tab strip
                        // without leaving 没入).
                        terminal::pane::PaneStep::NewAgentTab => {
                            if !pool.activate_agent(dir) {
                                pool.add_pane(
                                    term,
                                    dir,
                                    terminal::tabs::PaneKind::Agent,
                                    later_initial,
                                    cli,
                                    &label,
                                )?;
                            }
                            // Jumped to / opened the agent pane: the next loop pass
                            // drives that pane and republishes the tab strip.
                        }
                        // `Ctrl-O x` / `Alt-x`: close the active tab and keep driving
                        // the surviving pane that slides into the active slot (a
                        // different shell); when it was the last pane the session
                        // empties, so drop back to 在席 — the same handling as a shell
                        // that exits on its own (`Closed`).
                        terminal::pane::PaneStep::CloseTab => {
                            if !pool.close_active(dir, &label) {
                                return Ok(PaneExit::Closed);
                            }
                        }
                        // `Ctrl-^`: leave the pane to jump to the previously focused
                        // session; the event loop re-roots on it (attaching when live),
                        // leaving every pane alive in the pool (like `Ctrl-O`).
                        terminal::pane::PaneStep::PrevSession => {
                            return Ok(PaneExit::ToPreviousSession)
                        }
                        // A double click on a sidebar session row: leave the pane so
                        // the event loop re-roots on that focus row (attaching when
                        // live), every pane staying alive in the pool (like `Ctrl-^`).
                        terminal::pane::PaneStep::ToSession(row) => {
                            return Ok(PaneExit::ToSession(row))
                        }
                        // `Ctrl-Q`: leave 没入 to quit usagi. Every pane stays alive in
                        // the pool; the event loop raises the quit-confirmation modal.
                        terminal::pane::PaneStep::Quit => return Ok(PaneExit::Quit),
                        // The active pane's shell exited: drop it; keep driving when
                        // a pane remains, else fall to 在席.
                        terminal::pane::PaneStep::Closed => {
                            if !pool.close_active(dir, &label) {
                                return Ok(PaneExit::Closed);
                            }
                            // The surviving pane that slides into the active slot is
                            // driven on the next loop pass.
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
            // Persist this session's open panes (or clear when the last one closed) so
            // the next startup restores them — an agent resumes its conversation, a
            // terminal reopens a fresh shell. Done here, where the pane yields control
            // and the pool reflects the current set, so the on-disk snapshot tracks
            // every add / close. Best-effort: a write failure just means no restore.
            if restore_panes_enabled {
                match pool.snapshot_open_panes_for(dir) {
                    Some((active, panes)) => {
                        let _ = crate::infrastructure::open_panes_store::save(dir, active, &panes);
                    }
                    None => crate::infrastructure::open_panes_store::clear(dir),
                }
            }
            // Leaving only to edit the note (`Ctrl-E` → `PaneExit::OpenNote`) keeps
            // the last screen snapshot: the note editor floats over the right pane,
            // so the live terminal stays visible behind it, and the event loop
            // re-attaches the moment the editor closes. Every other exit clears it.
            if !matches!(result, Ok(PaneExit::OpenNote)) {
                home.clear_terminal_surface();
            }
            // The user may have committed / pushed / merged while in the pane, so
            // re-sync the worktree statuses now that they have left it. The sync
            // shells out to `git status` for every worktree and waits on the
            // cross-process state lock, which is slow precisely when several sessions
            // are running agents — so run it off the loop thread instead of freezing
            // the detach here. The refreshed list is published to `sessions_refresh`
            // for the event loop to apply on a later frame (keeping the cursor where
            // it is); until then the just-left statuses stay on screen. The worker is
            // tracked so a sync in flight at quit finishes its `state.json` write.
            // Best-effort: a sync failure simply leaves the last-known statuses.
            let refresh_handle = sessions_refresh.clone();
            let refresh_root = terminal_root.clone();
            track_worker(
                &workers,
                std::thread::spawn(move || {
                    if let Some(sessions) = reload_sessions(&refresh_root) {
                        refresh_handle.set(sessions);
                    }
                }),
            );
            // A launch / pane failure is surfaced and persisted by the event loop's
            // single error sink: `open_pane` logs the failure through
            // `HomeState::log_error`, which both shows it and writes it to the daily
            // log file. No separate `ErrorLog::record` here, so the failure is recorded
            // exactly once, by the same path as every other on-screen operation error.
            result
        };

    // Snapshot the selected session's live terminal for the sidebar's right-pane
    // preview (the tab-like view), or `None` when it has no running shell/agent.
    let mut preview = |dir: &Path,
                       sidebar: crate::domain::settings::Sidebar|
     -> Option<crate::presentation::tui::home::terminal::view::TerminalView> {
        pool.borrow_mut().snapshot(term, dir, sidebar)
    };

    // Read (and optionally navigate) a session's tabs from 切替: `←`/`→` pass a
    // `TabNav` to move the active tab, and the loop reads the strip (`None`) each
    // frame to draw it above the preview. Both go through the same pool the pane
    // driver uses, so a tab moved here is the one re-attaching reveals.
    let mut tab_op = |dir: &Path, nav: Option<terminal::tabs::TabNav>| -> (Vec<String>, usize) {
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

    let mut tab_action =
        |home: &mut HomeState, dir: &Path, tab: usize, action: event::TabMenuAction| {
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
            match action {
                event::TabMenuAction::Move(swap) => {
                    pool.move_tab_by(dir, tab, swap);
                }
                event::TabMenuAction::Rename(name) => {
                    pool.rename_tab(dir, tab, &name);
                }
                event::TabMenuAction::Close => {
                    pool.close_tab(dir, tab, &label);
                }
            }
            if restore_panes_enabled {
                match pool.snapshot_open_panes_for(dir) {
                    Some((active, panes)) => {
                        let _ = crate::infrastructure::open_panes_store::save(dir, active, &panes);
                    }
                    None => crate::infrastructure::open_panes_store::clear(dir),
                }
            }
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
            // Back to home: re-read the (possibly changed) Session Action UI, the
            // 没入 key scheme, and local LLM availability so the 在席 surface, the
            // pane's key handling, and the `ai` command reflect the edit without
            // reopening the home screen.
            crate::presentation::tui::config::Outcome::Back => {
                let settings = effective_settings(&config_root);
                Ok(Some(event::ConfigReload {
                    session_action_ui: settings.session_action_ui,
                    key_scheme: settings.key_scheme,
                    ai_available: local_llm_available(&settings),
                }))
            }
            crate::presentation::tui::config::Outcome::Quit => Ok(None),
        }
    };

    // Persist where the user is when they quit — the focused session and how
    // deeply they were engaged with it — so the next launch restores it alongside
    // the panes. Gated by the same setting as the pane restore (the two are one
    // "restore my session state" feature). Best-effort: a write failure just means
    // the next launch opens in the default 切替.
    let resume_root = workspace.path.clone();
    let mut save_resume = move |session: &str, level: ResumeLevel| {
        if !restore_panes_enabled {
            return;
        }
        use crate::infrastructure::resume_focus_store::StoredEngagement;
        let engagement = match level {
            ResumeLevel::Switch => StoredEngagement::Switch,
            ResumeLevel::Focus => StoredEngagement::Focus,
            ResumeLevel::Attached => StoredEngagement::Attached,
        };
        let _ = crate::infrastructure::resume_focus_store::save(&resume_root, session, engagement);
    };

    // Flush the freshness ("heat") timestamps gathered while the screen ran into
    // `state.json` on quit, so the sidebar dots survive a restart. Best-effort:
    // a write failure just means the dots reset to creation time next launch.
    let last_active_root = workspace.path.clone();
    let mut save_last_active = move |pairs: &[(String, chrono::DateTime<chrono::Utc>)]| {
        let _ = crate::usecase::session::persist_last_active(&last_active_root, pairs);
    };

    // Launch the self-update on a background thread when the user confirms the
    // update notice: re-run the documented install script (downloading the latest
    // release over `bash -c "curl … | bash"`) and surface its progress as the
    // shared loading rabbit, finishing with a restart prompt. Runs off-thread so a
    // slow download never blocks the screen; a second click while one is in flight
    // is ignored by the handle's `begin` guard.
    let mut dispatch_update = || {
        let handle = crate::presentation::tui::install_task::handle();
        if !handle.begin("アップデート中…") {
            return;
        }
        std::thread::spawn(move || {
            let (ok, message) = crate::usecase::self_update::run(
                &crate::usecase::doctor::SystemRunner,
                env!("CARGO_PKG_REPOSITORY"),
            );
            handle.finish(ok, message);
        });
    };

    // `unite add <name>`: resolve a registered workspace by name and load its
    // recorded sessions into a group to stack into the view. Reads the registry and
    // the workspace's `state.json`; an unknown name is reported back to log.
    let mut unite_resolve = |name: &str| -> std::result::Result<state::GroupSource, String> {
        let storage = crate::infrastructure::storage::Storage::open_default()
            .map_err(|e| format!("failed to open storage: {e}"))?;
        let ws = crate::usecase::workspace::overviews(&storage)
            .map_err(|e| format!("failed to load workspaces: {e}"))?
            .into_iter()
            .map(|o| o.workspace)
            .find(|w| w.name == name)
            .ok_or_else(|| format!("no workspace named \"{name}\""))?;
        let (sessions, _) =
            crate::usecase::workspace_state::recorded_sessions_for_display(&ws.path);
        let root_note = crate::usecase::workspace_state::recorded_root_note(&ws.path);
        Ok(state::GroupSource {
            name: ws.name,
            root_path: ws.path,
            root_note,
            sessions,
        })
    };

    // Open a PR clicked in the pinned popup in the platform's default browser — the
    // same detached, best-effort spawn the immersive pane uses for a clicked link,
    // so a missing opener or a spawn failure never disturbs the screen.
    let mut open_url = |url: &str| {
        use std::process::{Command, Stdio};
        let argv = terminal::link::open_command(url);
        if let Some((cmd, rest)) = argv.split_first() {
            let _ = Command::new(cmd)
                .args(rest)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
    };

    let mut wiring = event::Wiring {
        interaction_epoch: 0,
        workspace_root: &workspace.path,
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename_display,
        set_note: &mut set_note,
        reorder_session: &mut reorder_session,
        dispatch_remove: &mut dispatch_remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict_pool,
        existing_branches: &mut existing_branches,
        open_terminal: &mut open_terminal,
        open_url: &mut open_url,
        open_config: &mut open_config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close_tab,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
    };
    let outcome = event::event_loop(
        term,
        &mut reader,
        state,
        &monitor,
        &update,
        &sessions_refresh,
        &ai_available,
        &installed_agents,
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

/// Restore each session's persisted panes into the pool on startup, in the
/// background (nothing is attached yet): a terminal pane reopens a fresh shell, an
/// agent pane relaunches its CLI — resuming the conversation when one exists, so it
/// picks up where it left off. Snapshots are read from
/// [`open_panes_store`](crate::infrastructure::open_panes_store), keyed by worktree,
/// for the workspace root (the ⌂ root row) and every session worktree.
///
/// Best-effort throughout: a missing snapshot skips the session and a failed spawn
/// skips that one pane, so a partial restore never blocks the screen from opening.
fn restore_open_panes(
    term: &Term,
    state: &HomeState,
    pool: &std::cell::RefCell<terminal::pool::TerminalPool>,
    agent_wiring: &crate::domain::agent::AgentWiring,
    default_cli: crate::domain::settings::AgentCli,
) {
    use crate::infrastructure::open_panes_store::{self, StoredPaneKind};
    use terminal::tabs::PaneKind;

    // The dirs a snapshot may be keyed by — the workspace root and each session
    // worktree — paired with the label shown in their waiting notification. Deduped
    // so a path is never restored twice.
    let mut dirs: Vec<(PathBuf, String)> = Vec::new();
    let root = state.root_path().to_path_buf();
    if !root.as_os_str().is_empty() {
        dirs.push((root, "root".to_string()));
    }
    for wt in state.list().worktrees() {
        if dirs.iter().any(|(d, _)| d == &wt.path) {
            continue;
        }
        dirs.push((wt.path.clone(), state::worktree_name(wt).to_string()));
    }

    for (dir, label) in dirs {
        let Some(snapshot) = open_panes_store::load(&dir) else {
            continue;
        };
        for pane in &snapshot.panes {
            let spawned = match pane.kind {
                StoredPaneKind::Terminal => pool.borrow_mut().add_pane(
                    term,
                    &dir,
                    PaneKind::Terminal,
                    None,
                    default_cli,
                    &label,
                ),
                StoredPaneKind::Agent => {
                    let cli = pane.cli.unwrap_or(default_cli);
                    let agent = crate::infrastructure::agent::agent_for(cli);
                    // Resume the conversation when one exists so the agent continues
                    // where it left off rather than starting over.
                    let resume = agent.has_resumable_session(&dir);
                    let command = agent.launch_command(agent_wiring, resume, None);
                    pool.borrow_mut().add_pane(
                        term,
                        &dir,
                        PaneKind::Agent,
                        Some(&command),
                        cli,
                        &label,
                    )
                }
            };
            // A failed spawn just skips that pane; the rest still restore.
            if spawned.is_ok() {
                if let Some(label) = pane.label.as_deref() {
                    let (labels, _) = pool.borrow().tabs(&dir);
                    if let Some(index) = labels.len().checked_sub(1) {
                        let _ = pool.borrow_mut().rename_tab(&dir, index, label);
                    }
                }
            }
        }
        // Re-select the tab that was active when the snapshot was taken.
        pool.borrow_mut().set_active(&dir, snapshot.active);
    }
}

/// Create a session on a worker thread: run the git / filesystem work and build
/// the [`Completion`](tasks::Completion) the event loop applies (the success or
/// failure line, and the refreshed sessions read back with each worktree's git
/// status). The `bool` is whether it succeeded, for the task row's mark.
fn run_create(root: &Path, name: &str, interaction_epoch: u64) -> (bool, tasks::Completion) {
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
                target_root: Some(root.to_path_buf()),
                evict: None,
                // Drop straight into 在席 (Focus) on the new session once the event
                // loop applies this — the user just asked for it, so operate it
                // without making them navigate over. (MCP creates never reach here.)
                focus: Some(tasks::AutoFocus {
                    name: created.name.clone(),
                    interaction_epoch,
                }),
            },
        ),
        // The failure line is recorded to the daily log when the event loop applies
        // this completion (`apply_task_completion` routes error lines through the
        // screen's logger), so there is no separate `ErrorLog::record` here.
        Err(e) => (
            false,
            tasks::Completion {
                line: LogLine::error(format!("session failed: {e}")),
                sessions: None,
                target_root: Some(root.to_path_buf()),
                evict: None,
                focus: None,
            },
        ),
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
    focus: Option<tasks::AutoFocus>,
) -> (bool, tasks::Completion) {
    match crate::usecase::session::remove(root, name, force, agent) {
        Ok(outcome) if outcome.removed => (
            true,
            tasks::Completion {
                line: LogLine::output(format!("Removed session \"{name}\" 🧹")),
                sessions: reload_sessions(root),
                target_root: Some(root.to_path_buf()),
                evict: Some(
                    root.join(crate::infrastructure::repo_paths::STATE_DIR)
                        .join(crate::infrastructure::repo_paths::SESSIONS_DIR)
                        .join(name),
                ),
                focus,
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
                    target_root: Some(root.to_path_buf()),
                    evict: None,
                    focus: None,
                },
            )
        }
        // As with `run_create`, the failure line is persisted when the event loop
        // applies this completion through the screen's logger — no direct
        // `ErrorLog::record` here.
        Err(e) => (
            false,
            tasks::Completion {
                line: LogLine::error(format!("session remove failed: {e}")),
                sessions: None,
                target_root: Some(root.to_path_buf()),
                evict: None,
                focus: None,
            },
        ),
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
