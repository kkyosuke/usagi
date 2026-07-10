use super::super::oneshot::OneShot;
use super::super::state::{GroupSource, LogLine};
use super::super::tasks::AutoFocus;
use super::super::terminal::tabs::TabNav;
use super::*;
use crate::domain::settings::{AgentCli, SessionActionUi};
use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};
use crate::presentation::tui::io::screen::{ClickEvent, Input, ScrollEvent};
use chrono::{DateTime, Local, TimeZone, Utc};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// A [`ConfigReload`] carrying `ui` — the shape the config-close callback returns
/// in tests that only care about the 集中 surface.
fn reload(ui: SessionActionUi) -> ConfigReload {
    ConfigReload {
        session_action_ui: ui,
        key_scheme: crate::domain::settings::KeyScheme::default(),
        agent_cli: AgentCli::default(),
        ai_available: false,
    }
}

/// A `unite_resolve` fake that reports no workspace, for the loop tests that do
/// not exercise `unite add` (the dispatch path has its own dedicated test).
fn no_unite_resolve(name: &str) -> std::result::Result<GroupSource, String> {
    Err(format!("no workspace named \"{name}\""))
}

fn noop_create(_: &str) -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("created"),
        sessions: None,
        select: None,
        root_note: None,
    }
}

fn noop_remove(_: &str, _: bool) -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("removed"),
        sessions: None,
        select: None,
        root_note: None,
    }
}

fn noop_rename(_: &str, _: &str) -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("renamed"),
        sessions: None,
        select: None,
        root_note: None,
    }
}

fn noop_set_note(_: &str, _: &str) -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("note saved"),
        sessions: None,
        select: None,
        root_note: None,
    }
}

fn noop_set_todos() -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("todos saved"),
        sessions: None,
        select: None,
        root_note: None,
    }
}

fn noop_set_label(_: &str, _: Option<&str>) -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("label saved"),
        sessions: None,
        select: None,
        root_note: None,
    }
}

/// A key source that replays a scripted sequence of results.
struct ScriptedReader {
    keys: VecDeque<io::Result<Key>>,
}

impl ScriptedReader {
    fn new(keys: Vec<io::Result<Key>>) -> Self {
        Self { keys: keys.into() }
    }
}

impl KeyReader for ScriptedReader {
    fn read_key(&mut self) -> io::Result<Key> {
        // Default to Ctrl-C so a test can never spin forever: Esc is inert at the
        // base 選択, so Ctrl-C (which quits when no session is live, as in these
        // tests) is the terminator the loop falls back to.
        self.keys.pop_front().unwrap_or(Ok(Key::CtrlC))
    }
}

/// A source that replays a scripted run of full [`Input`] events — keys, scrolls,
/// and clicks — for the tests that exercise mouse handling (the key-only
/// [`ScriptedReader`] can only feed keys). Like it, drained reads default to
/// Ctrl-C so a test can never spin forever.
struct InputReader {
    inputs: VecDeque<io::Result<Input>>,
}

impl InputReader {
    fn new(inputs: Vec<io::Result<Input>>) -> Self {
        Self {
            inputs: inputs.into(),
        }
    }
}

impl KeyReader for InputReader {
    fn read_key(&mut self) -> io::Result<Key> {
        // The loop only reads inputs; surface a queued key for completeness, else
        // the Ctrl-C terminator.
        match self.inputs.pop_front() {
            Some(Ok(Input::Key(key))) => Ok(key),
            _ => Ok(Key::CtrlC),
        }
    }

    fn read_input(&mut self) -> io::Result<Input> {
        self.inputs
            .pop_front()
            .unwrap_or(Ok(Input::Key(Key::CtrlC)))
    }

    // Mirror the real terminal reader: the animate path (a live session, a mascot
    // blink kicked by the last keypress) reads through the timeout, so it must
    // drain the same scripted queue rather than the key-only default.
    fn read_input_timeout(&mut self, _timeout: Duration) -> io::Result<Option<Input>> {
        Ok(Some(self.read_input()?))
    }
}

fn worktree(branch: Option<&str>, path: &str) -> WorktreeState {
    WorktreeState {
        branch: branch.map(|b| b.to_string()),
        path: PathBuf::from(path),
        head: "abc1234".to_string(),
        primary: false,
        upstream: None,
        status: BranchStatus::Local,
        diff: None,
        ahead_behind: None,
        pr: Vec::new(),
        updated_at: Utc::now(),
    }
}

fn sample_state() -> HomeState {
    HomeState::new(
        "usagi",
        vec![
            worktree(Some("main"), "/r/main"),
            worktree(Some("feat"), "/r/feat"),
        ],
        None,
    )
}

fn prompt_state() -> HomeState {
    let mut state = sample_state();
    state.set_session_action_ui(SessionActionUi::Prompt);
    state
}

/// A `open_terminal` callback reporting the shell closed (one pane iteration).
fn noop_open(_: &mut HomeState, _: &Path, _: bool, _: bool) -> Result<PaneExit> {
    Ok(PaneExit::Closed)
}

fn noop_config(_: &Term) -> Result<Option<ConfigReload>> {
    Ok(Some(reload(SessionActionUi::Menu)))
}

/// A test `chat_ask` hook that echoes the prompt back on a ready channel, so a
/// submitted chat line drains to a reply on the next loop pass without a model
/// runtime. Tests that need a withheld / failed reply build their own.
fn ready_chat_ask(prompt: String) -> std::sync::mpsc::Receiver<Result<String, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = tx.send(Ok(format!("echo: {prompt}")));
    rx
}

fn noop_preview(_: &Path, _: Sidebar) -> Option<TerminalView> {
    None
}

/// A `tab_op` callback with no panes: navigation is a no-op and the strip is
/// empty, for the tests that never exercise tabs.
fn noop_tab_op(_: &Path, _: Option<TabNav>) -> (Vec<String>, usize) {
    (Vec::new(), 0)
}

/// A `close_tab` callback that does nothing, for the tests that never close a
/// tab from 選択.
fn noop_close(_: &mut HomeState, _: &Path) {}

/// A `reorder_session` callback reporting nothing moved, for the tests that never
/// reorder.
fn noop_reorder(_: &str, _: bool) -> SessionReorder {
    SessionReorder::Stationary
}

/// An `autostart_queued` callback that starts nothing, for the loop tests that do
/// not exercise queued-prompt autostart (its apply path is covered directly in
/// [`apply_autostart`] tests).
fn noop_autostart(_: &HomeState) -> Vec<String> {
    Vec::new()
}

/// A `broadcast_wake` callback that reports no running agents, for loop tests
/// that do not exercise the wake broadcast.
fn noop_broadcast_wake(_: &HomeState) -> usize {
    0
}

fn live_preview(_: &Path, _: Sidebar) -> Option<TerminalView> {
    Some(TerminalView::from_rows(vec!["live".to_string()], None))
}

fn noop_persist(_: &str) {}

fn noop_persist_entry(_: &crate::domain::history::HistoryEntry) {}

/// A no-op browser launcher for the loop fakes: clicking a PR in the popup shells
/// out for real only in production, so the tests just drop the URL.
fn noop_open_url(_: &str) {}

/// Background-pane hooks for the direct-`Wiring` loop tests that do not exercise
/// the loading-tab flow: `start_pending_spawn` reports `Reused` (a launch falls
/// back to a synchronous re-attach) and the pollers report nothing pending. The
/// flow itself is covered by the dedicated `background_tab` tests, which supply
/// capturing versions.
fn noop_start_pending_spawn(_: &mut HomeState, _: &Path, _: bool) -> anyhow::Result<StartPending> {
    Ok(StartPending::Reused)
}

fn noop_poll_pending_spawn(_: &Path) -> PendingPoll {
    PendingPoll::Gone
}

fn noop_activate_pending(_: &Path) -> bool {
    false
}

fn noop_clear_pending_spawn() {}

fn no_branches() -> Vec<String> {
    Vec::new()
}

/// Run the loop with all-default callbacks (idle preview, no-op pane).
fn run(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    run_full(
        keys,
        state,
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
}

/// Run the loop with all-default callbacks but a real `workspace_root`, so the
/// `preview` command's file read resolves against an on-disk directory.
fn run_at(keys: Vec<io::Result<Key>>, mut state: HomeState, root: &Path) -> Result<Outcome> {
    // In production the primary workspace's root path and the wiring's
    // `workspace_root` are the same directory, so mirror that here: the env editor
    // (and other cursor-group-scoped commands) resolve their target from the
    // state's root path, which must match `root` for the on-disk assertions.
    state.set_root_path(root);
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    event_loop_compat(
        &term,
        &mut reader,
        state,
        root,
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
}

/// Run the loop with all-default callbacks but every session live.
fn run_live(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    run_full(
        keys,
        state,
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_full(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    create_session: &mut dyn FnMut(&str) -> SessionOutcome,
    preview: &mut dyn FnMut(&Path, Sidebar) -> Option<TerminalView>,
    open_config: &mut dyn FnMut(&Term) -> Result<Option<ConfigReload>>,
) -> Result<Outcome> {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut branches: fn() -> Vec<String> = no_branches;
    event_loop_compat(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        create_session,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove_session,
        &mut branches,
        open_terminal,
        open_config,
        preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
}

fn run_full_external(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    open_external_terminal: &mut dyn FnMut(&Path) -> std::result::Result<(), String>,
) -> Result<Outcome> {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let tasks = TaskHandle::new();
    let mut persist: fn(&crate::domain::history::HistoryEntry) = noop_persist_entry;
    let mut dispatch_create = |_: &Path, _: &str, _: u64| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut set_todos_fake =
        |_: &Path, _: &str, _: &[crate::domain::workspace_state::SessionTodo]| noop_set_todos();
    let mut set_label_fake = |_: &Path, n: &str, id: Option<&str>| noop_set_label(n, id);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut dispatch_remove = |_: &Path, _: &str, _: bool, _: Option<AutoFocus>| {};
    let mut evict = |_: &Path| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut open_url: fn(&str) = noop_open_url;
    let mut dispatch_update = || {};
    let mut unite_resolve: fn(&str) -> std::result::Result<GroupSource, String> = no_unite_resolve;
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    let mut chat_ask = ready_chat_ask;
    let mut start_pending_spawn: fn(&mut HomeState, &Path, bool) -> anyhow::Result<StartPending> =
        noop_start_pending_spawn;
    let mut poll_pending_spawn: fn(&Path) -> PendingPoll = noop_poll_pending_spawn;
    let mut activate_pending: fn(&Path) -> bool = noop_activate_pending;
    let mut clear_pending_spawn: fn() = noop_clear_pending_spawn;
    let mut autostart_queued = noop_autostart as fn(&HomeState) -> Vec<String>;
    let mut broadcast_wake = noop_broadcast_wake as fn(&HomeState) -> usize;
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions: false,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        set_todos: &mut set_todos_fake,
        set_label: &mut set_label_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut dispatch_remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal,
        start_pending_spawn: &mut start_pending_spawn,
        poll_pending_spawn: &mut poll_pending_spawn,
        activate_pending: &mut activate_pending,
        clear_pending_spawn: &mut clear_pending_spawn,
        open_url: &mut open_url,
        open_external_terminal,
        open_config: &mut config,
        chat_ask: &mut chat_ask,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
        autostart_queued: &mut autostart_queued,
        broadcast_wake: &mut broadcast_wake,
    };
    event_loop(
        &term,
        &mut reader,
        state,
        &monitor,
        &UpdateHandle::new(),
        &SessionsRefreshHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &tasks,
        &mut wiring,
    )
}

/// Like [`run_full`] but with a custom `tab_op`, so a test can mirror production
/// where 選択 / 集中 republish the focused session's live pane strip each frame
/// (the default [`run_full`] uses [`noop_tab_op`], which never publishes a strip).
fn run_full_tabs(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path, Sidebar) -> Option<TerminalView>,
    tab_op: &mut TabOp<'_>,
) -> Result<Outcome> {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut branches: fn() -> Vec<String> = no_branches;
    event_loop_compat(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove_session,
        &mut branches,
        open_terminal,
        &mut config,
        preview,
        tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
}

/// Run the loop with a monitor reporting a live session, so `Ctrl-C` raises
/// the quit-confirmation modal instead of quitting outright. `persist`
/// records the commands run, so a test can prove the screen kept running
/// after the modal was cancelled.
fn run_with_live_monitor(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    persist: &mut dyn FnMut(&str),
) -> Result<Outcome> {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::with_live(vec![PathBuf::from("/r/main")]);
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    event_loop_compat(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove_session,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
}

fn typed(s: &str) -> Vec<io::Result<Key>> {
    s.chars().map(|c| Ok(Key::Char(c))).collect()
}

/// A scripted run of a workspace command from the (now default) 選択: a leading
/// `:` opens the command palette, then `s` is typed into it. Without the `:` the
/// characters would hit Overview navigation instead of the command line.
fn cmd(s: &str) -> Vec<io::Result<Key>> {
    let mut keys = vec![Ok(Key::Char(':'))];
    keys.extend(typed(s));
    keys
}

fn state_with_sessions(names: &[&str]) -> HomeState {
    let mut state = sample_state();
    let sessions = names
        .iter()
        .map(|n| SessionRecord {
            todos: Vec::new(),
            decisions: Vec::new(),
            name: n.to_string(),
            display_name: None,
            note: None,
            label_id: None,
            agent: Default::default(),
            origin: Default::default(),
            started_from: None,
            root: PathBuf::from(format!("/ws/.usagi/sessions/{n}")),
            worktrees: vec![worktree(Some(n), &format!("/ws/{n}"))],
            created_at: Utc::now(),
            last_active: None,
        })
        .collect();
    state.restore_sessions(sessions);
    state
}

/// Run the loop with a preset installed-agent startup probe, all other callbacks
/// no-op, quitting on the scripted keys — so the loop's probe-drain path is
/// exercised. (The entry git-sync feeds the same `SessionsRefreshHandle` the
/// pane-exit sync uses; its apply path is covered by
/// `a_background_refresh_updates_the_session_list_exactly_once`.)
fn run_with_startup_probes(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    installed_agents: &OneShot<Vec<AgentCli>>,
) -> Result<Outcome> {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    event_loop_compat(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        installed_agents,
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
}

/// Capture the directories `open_terminal` is driven against, attaching (closing
/// at once) for each, and run the loop with a live preview so every focus attaches.
fn run_capturing_attached_dirs(keys: Vec<io::Result<Key>>) -> Vec<PathBuf> {
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, _a: bool, _n: bool| {
        opened.borrow_mut().push(d.to_path_buf());
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    assert!(matches!(
        run_full(
            keys,
            sample_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    opened.into_inner()
}

/// Capture the directories `open_terminal` is driven against while the loop is
/// driven by scripted [`Input`] events (mouse tests), attaching (closing at once)
/// for each, with a live preview so every focus attaches. `state` lets a test set
/// up the sidebar / mode it clicks in.
fn run_capturing_attached_dirs_for_inputs(
    inputs: Vec<io::Result<Input>>,
    state: HomeState,
) -> Vec<PathBuf> {
    let term = Term::stdout();
    let mut reader = InputReader::new(inputs);
    let monitor = MonitorHandle::detached();
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, _a: bool, _n: bool| {
        opened.borrow_mut().push(d.to_path_buf());
        Ok(PaneExit::Closed)
    };
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    opened.into_inner()
}

fn config_keys() -> Vec<io::Result<Key>> {
    let mut keys = cmd("config");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    keys
}

/// Drive the loop with scripted [`Input`] events (mouse tests) and a recording
/// create callback, returning the session names creation was dispatched for.
fn run_capturing_creates_for_inputs(
    inputs: Vec<io::Result<Input>>,
    state: HomeState,
) -> Vec<String> {
    let term = Term::stdout();
    let mut reader = InputReader::new(inputs);
    let monitor = MonitorHandle::detached();
    let created = RefCell::new(Vec::new());
    let mut create = |name: &str| {
        created.borrow_mut().push(name.to_string());
        noop_create(name)
    };
    let mut persist: fn(&str) = noop_persist;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    created.into_inner()
}

/// Drive `event_loop` with a recording rename callback, returning the (target,
/// label) pairs it received and the final outcome.
fn run_recording_rename(keys: Vec<io::Result<Key>>) -> (Vec<(String, String)>, Outcome) {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let renamed = RefCell::new(Vec::new());
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut rename = |name: &str, label: &str| {
        renamed
            .borrow_mut()
            .push((name.to_string(), label.to_string()));
        noop_rename(name, label)
    };
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut rename,
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    (renamed.into_inner(), outcome)
}

/// Drive `event_loop` with a recording reorder callback that returns `response`
/// for every call, yielding the (name, up) pairs it received and the outcome.
fn run_recording_reorder(
    keys: Vec<io::Result<Key>>,
    response: SessionReorder,
) -> (Vec<(String, bool)>, Outcome) {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let moves = RefCell::new(Vec::new());
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut reorder = |name: &str, up: bool| {
        moves.borrow_mut().push((name.to_string(), up));
        response.clone()
    };
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut reorder,
    )
    .unwrap();
    (moves.into_inner(), outcome)
}

/// Run the loop against a monitor reporting `waiting` sessions, recording the
/// directory the preview is asked for each frame (the cursor row's). Lets a test
/// observe which session a row resolves to after the waiting-first sort reorders
/// the pane.
fn run_recording_previews(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    waiting: Vec<PathBuf>,
) -> Vec<PathBuf> {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::with_waiting(waiting);
    let previews = RefCell::new(Vec::new());
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview = |dir: &Path, _: Sidebar| -> Option<TerminalView> {
        previews.borrow_mut().push(dir.to_path_buf());
        None
    };
    event_loop_compat(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    previews.into_inner()
}

/// A reader that scripts the timeout reads (and blocking reads) separately, so
/// the loop's background-task animation path can be exercised directly.
struct TimeoutScript {
    timeouts: VecDeque<io::Result<Option<Key>>>,
    blocking: VecDeque<io::Result<Key>>,
}

impl KeyReader for TimeoutScript {
    fn read_key(&mut self) -> io::Result<Key> {
        self.blocking.pop_front().unwrap_or(Ok(Key::CtrlC))
    }
    fn read_key_timeout(&mut self, _t: std::time::Duration) -> io::Result<Option<Key>> {
        self.timeouts.pop_front().unwrap_or(Ok(Some(Key::CtrlC)))
    }
}

/// Run the real loop (not the compat shim) with a pre-seeded task handle and a
/// custom reader, so the background-task read and drain paths are exercised
/// directly. Every session callback is a no-op except the injected
/// `dispatch_remove` / `evict_pool`.
fn run_with_tasks(
    tasks: &TaskHandle,
    reader: &mut dyn KeyReader,
    mut dispatch_remove: impl FnMut(&str, bool),
    mut evict_pool: impl FnMut(&Path),
) -> Result<Outcome> {
    let term = Term::stdout();
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&crate::domain::history::HistoryEntry) = noop_persist_entry;
    let mut dispatch_create = |_: &Path, _: &str, _: u64| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut set_todos_fake =
        |_: &Path, _: &str, _: &[crate::domain::workspace_state::SessionTodo]| noop_set_todos();
    let mut set_label_fake = |_: &Path, n: &str, id: Option<&str>| noop_set_label(n, id);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut open_url: fn(&str) = noop_open_url;
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    let mut dispatch_update = || {};
    // The unite target root is irrelevant to this single-workspace fake, so wrap
    // the caller's removal hook to the production 3-arg shape, dropping the root.
    let mut dispatch_remove_w = |_: &Path, name: &str, force: bool, _| dispatch_remove(name, force);
    let mut unite_resolve: fn(&str) -> std::result::Result<GroupSource, String> = no_unite_resolve;
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    let mut chat_ask = ready_chat_ask;
    let mut start_pending_spawn: fn(&mut HomeState, &Path, bool) -> anyhow::Result<StartPending> =
        noop_start_pending_spawn;
    let mut poll_pending_spawn: fn(&Path) -> PendingPoll = noop_poll_pending_spawn;
    let mut activate_pending: fn(&Path) -> bool = noop_activate_pending;
    let mut clear_pending_spawn: fn() = noop_clear_pending_spawn;
    let mut autostart_queued = noop_autostart as fn(&HomeState) -> Vec<String>;
    let mut broadcast_wake = noop_broadcast_wake as fn(&HomeState) -> usize;
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions: false,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        set_todos: &mut set_todos_fake,
        set_label: &mut set_label_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut dispatch_remove_w,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict_pool,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        start_pending_spawn: &mut start_pending_spawn,
        poll_pending_spawn: &mut poll_pending_spawn,
        activate_pending: &mut activate_pending,
        clear_pending_spawn: &mut clear_pending_spawn,
        open_url: &mut open_url,
        open_external_terminal: &mut open_external_terminal,
        open_config: &mut config,
        chat_ask: &mut chat_ask,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
        autostart_queued: &mut autostart_queued,
        broadcast_wake: &mut broadcast_wake,
    };
    event_loop(
        &term,
        reader,
        sample_state(),
        &monitor,
        &UpdateHandle::new(),
        &SessionsRefreshHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        tasks,
        &mut wiring,
    )
}

/// Run the real loop with one live session and no install / task in flight, so
/// the loop animates purely because a session is live. Drives the given reader,
/// proving the loop wakes on the timeout tick (to reflect a background agent's
/// badge) instead of blocking on the next key.
fn run_with_live_session(reader: &mut dyn KeyReader) -> Result<Outcome> {
    run_with_state_monitor_watch(
        reader,
        sample_state(),
        MonitorHandle::with_live(vec![PathBuf::from("/r/main")]),
        false,
    )
}

fn run_with_pending_session(reader: &mut dyn KeyReader) -> Result<Outcome> {
    let mut state = sample_state();
    state.begin_pending_session(PathBuf::from("/r/main"), "new-session".to_string());
    run_with_state_monitor_watch(reader, state, MonitorHandle::detached(), false)
}

fn run_with_state_monitor_watch(
    reader: &mut dyn KeyReader,
    state: HomeState,
    monitor: MonitorHandle,
    watch_sessions: bool,
) -> Result<Outcome> {
    let term = Term::stdout();
    let tasks = TaskHandle::new();
    let mut persist: fn(&crate::domain::history::HistoryEntry) = noop_persist_entry;
    let mut dispatch_create = |_: &Path, _: &str, _: u64| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut dispatch_remove = |_: &Path, _: &str, _: bool, _| {};
    let mut evict = |_: &Path| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut set_todos_fake =
        |_: &Path, _: &str, _: &[crate::domain::workspace_state::SessionTodo]| noop_set_todos();
    let mut set_label_fake = |_: &Path, n: &str, id: Option<&str>| noop_set_label(n, id);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut open_url: fn(&str) = noop_open_url;
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    let mut dispatch_update = || {};
    let mut unite_resolve: fn(&str) -> std::result::Result<GroupSource, String> = no_unite_resolve;
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    let mut chat_ask = ready_chat_ask;
    let mut start_pending_spawn: fn(&mut HomeState, &Path, bool) -> anyhow::Result<StartPending> =
        noop_start_pending_spawn;
    let mut poll_pending_spawn: fn(&Path) -> PendingPoll = noop_poll_pending_spawn;
    let mut activate_pending: fn(&Path) -> bool = noop_activate_pending;
    let mut clear_pending_spawn: fn() = noop_clear_pending_spawn;
    let mut autostart_queued = noop_autostart as fn(&HomeState) -> Vec<String>;
    let mut broadcast_wake = noop_broadcast_wake as fn(&HomeState) -> usize;
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        set_todos: &mut set_todos_fake,
        set_label: &mut set_label_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut dispatch_remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        start_pending_spawn: &mut start_pending_spawn,
        poll_pending_spawn: &mut poll_pending_spawn,
        activate_pending: &mut activate_pending,
        clear_pending_spawn: &mut clear_pending_spawn,
        open_url: &mut open_url,
        open_external_terminal: &mut open_external_terminal,
        open_config: &mut config,
        chat_ask: &mut chat_ask,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
        autostart_queued: &mut autostart_queued,
        broadcast_wake: &mut broadcast_wake,
    };
    event_loop(
        &term,
        reader,
        state,
        &monitor,
        &UpdateHandle::new(),
        &SessionsRefreshHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &tasks,
        &mut wiring,
    )
}

/// Run the real loop idle — no live session and nothing in flight — but with
/// `watch_sessions` on, so it wakes on the watch tick to apply a session list a
/// background watcher published instead of blocking on the next key. Mirrors
/// [`run_with_live_session`], swapping the live monitor for a detached one and
/// turning the watcher flag on, so the loop's idle-watching read path is
/// exercised directly.
fn run_idle_watching(reader: &mut dyn KeyReader) -> Result<Outcome> {
    run_with_state_monitor_watch(reader, sample_state(), MonitorHandle::detached(), true)
}

/// Run the loop recording every `set_note` call, with a custom `open_terminal`
/// and `preview` so the 没入 (Attached) note flow can be exercised.
#[allow(clippy::too_many_arguments)]
fn run_notes(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    open: &mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path, Sidebar) -> Option<TerminalView>,
    set_note: &mut dyn FnMut(&str, &str) -> SessionOutcome,
) -> Result<Outcome> {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut branches: fn() -> Vec<String> = no_branches;
    event_loop_compat(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        set_note,
        &mut remove,
        &mut branches,
        open,
        &mut config,
        preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
}

/// The fragments a terminal sends for `Shift`+`<letter>` cursor key, reassembled
/// by `term_reader` into the single `UnknownEscSeq` the loop sees: `CSI 1 ; 2
/// <letter>`. `letter` is the CSI final byte (`C` right, `D` left, `A` up, `B`
/// down, `H` home, `F` end).
fn shift_arrow(letter: char) -> io::Result<Key> {
    Ok(Key::UnknownEscSeq(vec!['[', '1', ';', '2', letter]))
}

/// Map a string to the `:`-palette key sequence that types it then presses Enter,
/// so a test can run a full `:command` in one go.
fn typed_command(cmd: &str) -> Vec<io::Result<Key>> {
    let mut keys = vec![Ok(Key::Char(':'))];
    keys.extend(cmd.chars().map(|c| Ok(Key::Char(c))));
    keys.push(Ok(Key::Enter));
    keys
}

#[test]
fn unite_add_through_the_compat_shim_reports_an_unresolved_workspace() {
    // The compat shim's resolver always reports no match, so `:unite add` logs the
    // error and the loop keeps running until the reader's Ctrl-C fallback quits.
    let outcome = run(typed_command("unite add nope"), sample_state()).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
}

#[test]
fn unite_add_and_remove_run_through_the_palette() {
    // A capturing resolver: it loads "wsB" into a group and refuses anything else,
    // so the palette `unite add/remove` arms exercise every branch (add, duplicate,
    // remove, missing, and an unresolved name).
    let term = Term::stdout();
    let monitor = MonitorHandle::detached();
    let calls = std::cell::RefCell::new(Vec::<String>::new());
    let mut unite_resolve = |name: &str| -> std::result::Result<GroupSource, String> {
        calls.borrow_mut().push(name.to_string());
        if name == "wsB" {
            Ok(GroupSource {
                name: "wsB".to_string(),
                root_path: PathBuf::from("/wsB"),
                root_note: None,
                sessions: Vec::new(),
                issues: Vec::new(),
            })
        } else {
            Err(format!("no workspace named \"{name}\""))
        }
    };
    let mut keys = Vec::new();
    keys.extend(typed_command("unite add wsB")); // add → enters unite mode
    keys.extend(typed_command("unite add wsB")); // duplicate → refused
    keys.extend(typed_command("unite remove wsB")); // remove → back to single
    keys.extend(typed_command("unite remove wsB")); // missing → refused
    keys.extend(typed_command("unite add ghost")); // resolver error
    keys.push(Ok(Key::CtrlC));
    let mut reader = ScriptedReader::new(keys);

    let mut persist: fn(&crate::domain::history::HistoryEntry) = noop_persist_entry;
    let mut dispatch_create = |_: &Path, _: &str, _: u64| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut set_todos_fake =
        |_: &Path, _: &str, _: &[crate::domain::workspace_state::SessionTodo]| noop_set_todos();
    let mut set_label_fake = |_: &Path, n: &str, id: Option<&str>| noop_set_label(n, id);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut dispatch_remove = |_: &Path, _: &str, _: bool, _| {};
    let mut evict = |_: &Path| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut open_url: fn(&str) = noop_open_url;
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    let mut dispatch_update = || {};
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    let mut chat_ask = ready_chat_ask;
    let mut start_pending_spawn: fn(&mut HomeState, &Path, bool) -> anyhow::Result<StartPending> =
        noop_start_pending_spawn;
    let mut poll_pending_spawn: fn(&Path) -> PendingPoll = noop_poll_pending_spawn;
    let mut activate_pending: fn(&Path) -> bool = noop_activate_pending;
    let mut clear_pending_spawn: fn() = noop_clear_pending_spawn;
    let mut autostart_queued = noop_autostart as fn(&HomeState) -> Vec<String>;
    let mut broadcast_wake = noop_broadcast_wake as fn(&HomeState) -> usize;
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions: false,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        set_todos: &mut set_todos_fake,
        set_label: &mut set_label_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut dispatch_remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        start_pending_spawn: &mut start_pending_spawn,
        poll_pending_spawn: &mut poll_pending_spawn,
        activate_pending: &mut activate_pending,
        clear_pending_spawn: &mut clear_pending_spawn,
        open_url: &mut open_url,
        open_external_terminal: &mut open_external_terminal,
        open_config: &mut config,
        chat_ask: &mut chat_ask,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
        autostart_queued: &mut autostart_queued,
        broadcast_wake: &mut broadcast_wake,
    };
    let outcome = event_loop(
        &term,
        &mut reader,
        sample_state(),
        &monitor,
        &UpdateHandle::new(),
        &SessionsRefreshHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &TaskHandle::new(),
        &mut wiring,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // The resolver was consulted for both `add` calls and the unresolved name (not
    // for `remove`, which is pure state).
    assert_eq!(calls.into_inner(), vec!["wsB", "wsB", "ghost"]);
}

#[test]
fn selected_dir_roots_at_the_cursor_groups_workspace() {
    // A united home: primary "primary" (root only) plus an empty extra group "wsB".
    // Each expanded workspace owns a create row, so the flat rows are:
    //   0 = primary root, 1 = primary create, 2 = wsB root, 3 = wsB create.
    let mut state = HomeState::new("primary", Vec::new(), None);
    state.set_root_path("/primary");
    state.set_extra_groups(vec![GroupSource {
        name: "wsB".to_string(),
        root_path: PathBuf::from("/wsB"),
        root_note: None,
        sessions: Vec::new(),
        issues: Vec::new(),
    }]);
    // The primary group's root row uses the screen's base workspace root.
    state.overview_select(0);
    assert_eq!(
        selected_dir(&state, Path::new("/primary")),
        PathBuf::from("/primary")
    );
    // An extra group's root row roots at that group's own workspace.
    state.overview_select(2);
    assert_eq!(
        selected_dir(&state, Path::new("/primary")),
        PathBuf::from("/wsB")
    );
}

#[test]
fn pending_pr_link_updates_refresh_sidebar_rows() {
    let pr = crate::domain::workspace_state::PrLink::new(412, "https://github.com/o/r/pull/412");
    let monitor =
        MonitorHandle::with_pr_link_updates(vec![(PathBuf::from("/r/feat"), vec![pr.clone()])]);
    let mut state = sample_state();

    assert!(apply_pending_pr_links(&mut state, &monitor));
    assert_eq!(state.list().worktrees()[1].pr, vec![pr]);
    // The drain is one-shot; a second pass has nothing to apply and should not
    // force a repaint.
    assert!(!apply_pending_pr_links(&mut state, &monitor));
}

#[test]
fn apply_autostart_logs_started_panes_and_reports_change() {
    let mut state = sample_state();
    let before = state.log().len();

    // Nothing started (feature off / nothing queued): no log line, reports no
    // change, so the loop does not force a repaint on its account.
    let mut none = |_: &HomeState| Vec::<String>::new();
    assert!(!apply_autostart(&mut state, &mut none));
    assert_eq!(state.log().len(), before);

    // Two panes started: each returned line is appended to the command log, and it
    // reports a change so the loop repaints and the new sidebar badges show.
    let mut two = |_: &HomeState| {
        vec![
            "queued prompt auto-started for feat: do X".to_string(),
            "queued prompt auto-started for root: do Y".to_string(),
        ]
    };
    assert!(apply_autostart(&mut state, &mut two));
    assert_eq!(state.log().len(), before + 2);
}

#[test]
fn apply_due_wake_broadcasts_once_and_logs_count() {
    let mut state = sample_state();
    let now = Local
        .with_ymd_and_hms(2026, 7, 10, 14, 0, 0)
        .single()
        .unwrap();
    state.schedule_wake(now, 14, 30).unwrap();
    let before_due = Local
        .with_ymd_and_hms(2026, 7, 10, 14, 29, 59)
        .single()
        .unwrap();
    let due = Local
        .with_ymd_and_hms(2026, 7, 10, 14, 30, 0)
        .single()
        .unwrap();
    let calls = std::cell::Cell::new(0);
    let mut broadcast = |_: &HomeState| {
        calls.set(calls.get() + 1);
        2
    };

    assert!(!apply_due_wake(&mut state, before_due, &mut broadcast));
    assert_eq!(calls.get(), 0);
    assert!(apply_due_wake(&mut state, due, &mut broadcast));
    assert_eq!(calls.get(), 1);
    assert!(state
        .log()
        .last()
        .is_some_and(|line| line.text.contains("sent `continue` to 2")));
    // Consumed after the first due tick, so it does not fire again.
    assert!(!apply_due_wake(&mut state, due, &mut broadcast));
    assert_eq!(calls.get(), 1);
}

#[test]
fn apply_due_wake_logs_when_no_agents_are_running() {
    let mut state = sample_state();
    let now = Local
        .with_ymd_and_hms(2026, 7, 10, 14, 0, 0)
        .single()
        .unwrap();
    let due = Local
        .with_ymd_and_hms(2026, 7, 10, 14, 30, 0)
        .single()
        .unwrap();
    state.schedule_wake(now, 14, 30).unwrap();
    let mut broadcast = |_: &HomeState| 0usize;

    assert!(apply_due_wake(&mut state, due, &mut broadcast));
    assert!(state
        .log()
        .last()
        .is_some_and(|line| line.text.contains("no running agents")));
}

#[test]
fn size_changed_reports_a_resize_and_updates_the_memo() {
    let mut last = None;

    // The first pass has no previous size to differ from: not a resize (the
    // first frame paints via `force_paint` anyway), but the size is memoised.
    assert!(!size_changed(&mut last, (24, 80)));
    assert_eq!(last, Some((24, 80)));

    // The same size on a later pass is not a resize either.
    assert!(!size_changed(&mut last, (24, 80)));

    // A different size is: the loop must force a repaint past the quiet-選択
    // skip, and the memo moves so the next pass is quiet again.
    assert!(size_changed(&mut last, (30, 100)));
    assert_eq!(last, Some((30, 100)));
    assert!(!size_changed(&mut last, (30, 100)));
}

#[test]
fn run_wake_commands_schedules_and_cancels() {
    use chrono::Timelike;
    let now = Local::now();

    // Future time today
    let mut future_h = now.hour();
    let mut future_m = now.minute() + 2;
    if future_m >= 60 {
        future_h = (future_h + 1) % 24;
        future_m %= 60;
    }
    if future_h == 0 {
        future_h = 23;
        future_m = 59;
    }
    let future_time = format!("{:02}:{:02}", future_h, future_m);

    // Past time today
    let past_time = if now.hour() > 0 {
        format!("{:02}:00", now.hour() - 1)
    } else {
        "00:00".to_string()
    };

    // 1. Schedule wake successfully
    let mut keys = cmd(&format!("wake -t {}", future_time));
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    let outcome = run(keys, sample_state()).unwrap();
    assert!(matches!(outcome, Outcome::Quit));

    // 2. Schedule wake with past time (error)
    let mut keys = cmd(&format!("wake -t {}", past_time));
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    let outcome = run(keys, sample_state()).unwrap();
    assert!(matches!(outcome, Outcome::Quit));

    // 3. Cancel scheduled wake
    let mut state_with_wake = sample_state();
    let _ = state_with_wake.schedule_wake(now, future_h, future_m);
    let mut keys = cmd("wake cancel");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    let outcome = run(keys, state_with_wake).unwrap();
    assert!(matches!(outcome, Outcome::Quit));

    // 4. Cancel wake when none is scheduled
    let mut keys = cmd("wake cancel");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    let outcome = run(keys, sample_state()).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
}

#[test]
fn wake_scheduled_line_notes_a_replaced_wake() {
    let now = Local
        .with_ymd_and_hms(2026, 7, 10, 14, 0, 0)
        .single()
        .unwrap();
    let at = Local
        .with_ymd_and_hms(2026, 7, 10, 14, 30, 0)
        .single()
        .unwrap();

    // With no earlier wake, the line is just the confirmation and countdown.
    let fresh = super::handlers::wake_scheduled_line(now, at, None);
    assert!(fresh.contains("Wake scheduled for 14:30 (in 30m)"));
    assert!(!fresh.contains("replaced"));

    // Rescheduling over a pending wake notes the one it replaced, so the previous
    // time is never silently dropped.
    let previous = Local
        .with_ymd_and_hms(2026, 7, 10, 13, 0, 0)
        .single()
        .unwrap();
    let replaced = super::handlers::wake_scheduled_line(now, at, Some(previous));
    assert!(replaced.contains("Wake scheduled for 14:30 (in 30m)"));
    assert!(replaced.contains("(replaced earlier wake for 13:00)"));
}

#[test]
fn run_wake_in_schedules_a_relative_wake() {
    // `wake -i 30m` drives the relative-schedule handler arm end to end.
    let mut keys = cmd("wake -i 30m");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    let outcome = run(keys, sample_state()).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
}

#[test]
fn run_wake_status_reports_the_schedule() {
    use chrono::Timelike;
    let now = Local::now();

    // 1. `wake status` with nothing scheduled exercises the "none" branch.
    let mut keys = cmd("wake status");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    let outcome = run(keys, sample_state()).unwrap();
    assert!(matches!(outcome, Outcome::Quit));

    // 2. Bare `wake` with a pending wake exercises the "scheduled" branch (and the
    //    countdown formatting). Seed a future wake so it is still pending.
    let mut future_h = now.hour();
    let mut future_m = now.minute() + 2;
    if future_m >= 60 {
        future_h = (future_h + 1) % 24;
        future_m %= 60;
    }
    if future_h == 0 {
        future_h = 23;
        future_m = 59;
    }
    let mut state = sample_state();
    let _ = state.schedule_wake(now, future_h, future_m);
    let mut keys = cmd("wake");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    let outcome = run(keys, state).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
}

mod attached;
mod background_tab;
mod background_tasks;
mod clicks;
mod closeup_menu;
mod closeup_prompt;
mod config_switch;
mod ctrl_caret;
mod diff;
mod env_editor;
mod labels;
mod mascot_click;
mod notes;
mod overview_mode;
mod palette;
mod pr_popup;
mod quit_modal;
mod session_lifecycle;
mod startup;
mod update_modal;
