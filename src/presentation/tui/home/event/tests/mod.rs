use super::super::oneshot::OneShot;
use super::super::state::LogLine;
use super::super::terminal::tabs::TabNav;
use super::*;
use crate::domain::settings::SessionActionUi;
use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};
use chrono::Utc;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;

/// A [`ConfigReload`] carrying `ui` and the local LLM left unavailable — the
/// shape the config-close callback returns in tests that only care about the
/// 在席 surface.
fn reload(ui: SessionActionUi) -> ConfigReload {
    ConfigReload {
        session_action_ui: ui,
        ai_available: false,
    }
}

fn noop_create(_: &str) -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("created"),
        sessions: None,
        select: None,
    }
}

fn noop_remove(_: &str, _: bool) -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("removed"),
        sessions: None,
        select: None,
    }
}

fn noop_rename(_: &str, _: &str) -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("renamed"),
        sessions: None,
        select: None,
    }
}

fn noop_set_note(_: &str, _: &str) -> SessionOutcome {
    SessionOutcome {
        line: LogLine::output("note saved"),
        sessions: None,
        select: None,
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
        // base 切替, so Ctrl-C (which quits when no session is live, as in these
        // tests) is the terminator the loop falls back to.
        self.keys.pop_front().unwrap_or(Ok(Key::CtrlC))
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

fn noop_preview(_: &Path, _: Sidebar) -> Option<TerminalView> {
    None
}

/// A `tab_op` callback with no panes: navigation is a no-op and the strip is
/// empty, for the tests that never exercise tabs.
fn noop_tab_op(_: &Path, _: Option<TabNav>) -> (Vec<String>, usize) {
    (Vec::new(), 0)
}

/// A `close_tab` callback that does nothing, for the tests that never close a
/// tab from 切替.
fn noop_close(_: &mut HomeState, _: &Path) {}

/// A `reorder_session` callback reporting nothing moved, for the tests that never
/// reorder.
fn noop_reorder(_: &str, _: bool) -> SessionReorder {
    SessionReorder::Stationary
}

fn live_preview(_: &Path, _: Sidebar) -> Option<TerminalView> {
    Some(TerminalView::from_rows(vec!["live".to_string()], None))
}

fn noop_persist(_: &str) {}

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
fn run_at(keys: Vec<io::Result<Key>>, state: HomeState, root: &Path) -> Result<Outcome> {
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
        &OneShot::<bool>::new(),
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
        &OneShot::<bool>::new(),
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
        &OneShot::<bool>::new(),
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

/// A scripted run of a workspace command from the (now default) 切替: a leading
/// `:` opens the command palette, then `s` is typed into it. Without the `:` the
/// characters would hit Switch navigation instead of the command line.
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
            name: n.to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from(format!("/ws/.usagi/sessions/{n}")),
            worktrees: vec![worktree(Some(n), &format!("/ws/{n}"))],
            created_at: Utc::now(),
        })
        .collect();
    state.restore_sessions(sessions);
    state
}

/// Run the loop with a preset local-LLM probe one-shot, all other callbacks
/// no-op, quitting on the scripted keys — so the loop's drain of the probe is
/// exercised. (The entry git-sync feeds the same `SessionsRefreshHandle` the
/// pane-exit sync uses; its apply path is covered by
/// `a_background_refresh_updates_the_session_list_exactly_once`.)
fn run_with_ai_probe(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    ai_available: &OneShot<bool>,
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
        ai_available,
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

fn config_keys() -> Vec<io::Result<Key>> {
    let mut keys = cmd("config");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    keys
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
        &OneShot::<bool>::new(),
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
        &OneShot::<bool>::new(),
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
    let mut persist: fn(&str) = noop_persist;
    let mut dispatch_create = |_: &str| {};
    let mut rename: fn(&str, &str) -> SessionOutcome = noop_rename;
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut set_note_fake: fn(&str, &str) -> SessionOutcome = noop_set_note;
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut wiring = Wiring {
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut dispatch_remove,
        evict_pool: &mut evict_pool,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        open_config: &mut config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        save_resume: &mut save_resume,
    };
    event_loop(
        &term,
        reader,
        sample_state(),
        &monitor,
        &UpdateHandle::new(),
        &SessionsRefreshHandle::new(),
        &OneShot::<bool>::new(),
        tasks,
        &mut wiring,
    )
}

/// Run the real loop with one live session and no install / task in flight, so
/// the loop animates purely because a session is live. Drives the given reader,
/// proving the loop wakes on the timeout tick (to reflect a background agent's
/// badge) instead of blocking on the next key.
fn run_with_live_session(reader: &mut dyn KeyReader) -> Result<Outcome> {
    let term = Term::stdout();
    let monitor = MonitorHandle::with_live(vec![PathBuf::from("/r/main")]);
    let tasks = TaskHandle::new();
    let mut persist: fn(&str) = noop_persist;
    let mut dispatch_create = |_: &str| {};
    let mut rename: fn(&str, &str) -> SessionOutcome = noop_rename;
    let mut dispatch_remove = |_: &str, _: bool| {};
    let mut evict = |_: &Path| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut set_note_fake: fn(&str, &str) -> SessionOutcome = noop_set_note;
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut wiring = Wiring {
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut dispatch_remove,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        open_config: &mut config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        save_resume: &mut save_resume,
    };
    event_loop(
        &term,
        reader,
        sample_state(),
        &monitor,
        &UpdateHandle::new(),
        &SessionsRefreshHandle::new(),
        &OneShot::<bool>::new(),
        &tasks,
        &mut wiring,
    )
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
        &OneShot::<bool>::new(),
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

mod attached;
mod background_tasks;
mod config_switch;
mod ctrl_caret;
mod focus_menu;
mod focus_prompt;
mod notes;
mod palette;
mod quit_modal;
mod session_lifecycle;
mod startup;
mod switch_mode;
