use super::super::state::LogLine;
use super::*;
use crate::domain::settings::SessionActionUi;
use crate::domain::workspace_state::{BranchStatus, SessionRecord, WorktreeState};
use chrono::Utc;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::path::PathBuf;

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
        // Default to Ctrl-C so a test can never spin forever: Esc no longer
        // leaves Overview, so Ctrl-C (which quits when no session is live, as
        // in these tests) is the terminator the loop falls back to.
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
fn noop_open(_: &mut HomeState, _: &Path, _: bool) -> Result<PaneExit> {
    Ok(PaneExit::Closed)
}

fn noop_config(_: &Term) -> Result<Option<SessionActionUi>> {
    Ok(Some(SessionActionUi::Menu))
}

fn noop_preview(_: &Path) -> Option<TerminalView> {
    None
}

fn live_preview(_: &Path) -> Option<TerminalView> {
    Some(TerminalView::from_rows(vec!["live".to_string()], None))
}

fn noop_persist(_: &str) {}

fn no_branches() -> Vec<String> {
    Vec::new()
}

/// Run the loop with all-default callbacks (idle preview, no-op pane).
fn run(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    run_full(
        keys,
        state,
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
}

/// Run the loop with all-default callbacks but every session live.
fn run_live(keys: Vec<io::Result<Key>>, state: HomeState) -> Result<Outcome> {
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
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
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool) -> Result<PaneExit>,
    create_session: &mut dyn FnMut(&str) -> SessionOutcome,
    preview: &mut dyn FnMut(&Path) -> Option<TerminalView>,
    open_config: &mut dyn FnMut(&Term) -> Result<Option<SessionActionUi>>,
) -> Result<Outcome> {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut remove_session: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut branches: fn() -> Vec<String> = no_branches;
    event_loop(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &mut persist,
        create_session,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove_session,
        &mut branches,
        open_terminal,
        open_config,
        preview,
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
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
    event_loop(
        &term,
        &mut reader,
        state,
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove_session,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
    )
}

#[test]
fn a_populated_update_handle_is_read_before_painting() {
    // With the background check reporting a newer release, the loop reads the
    // handle each frame and renders the top-right notice. It still quits on the
    // trailing Ctrl-C, proving the update path does not disturb the loop.
    use crate::domain::version::Version;
    use crate::usecase::update_check::UpdateStatus;

    let term = Term::stdout();
    let mut reader = ScriptedReader::new(vec![Ok(Key::CtrlC)]);
    let monitor = MonitorHandle::detached();
    let update = UpdateHandle::new();
    update.set(UpdateStatus {
        current: Version::parse("0.0.1").unwrap(),
        latest: Version::parse("0.2.0").unwrap(),
    });
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &update,
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
}

fn typed(s: &str) -> Vec<io::Result<Key>> {
    s.chars().map(|c| Ok(Key::Char(c))).collect()
}

/// 切替 (Switch) reached from the 在席 prompt surface via `Ctrl-O`, then a
/// different session focused: the session changes as expected. Guards the
/// prompt-mode path of `focus_key`'s `Ctrl-O` handling (the menu path is covered
/// by [`focus_ctrl_o_opens_switch_then_esc_re_focuses`]).
#[test]
fn prompt_focus_ctrl_o_opens_switch_and_can_change_session() {
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    // Only `feat` has a live terminal, so focusing the idle root stays in 在席
    // (no auto-attach) until Ctrl-O reaches Switch and `feat` is selected.
    let mut preview = |p: &Path| {
        if p.to_string_lossy().contains("feat") {
            Some(TerminalView::from_rows(vec!["live".to_string()], None))
        } else {
            None
        }
    };
    let mut keys = typed("session switch root");
    keys.push(Ok(Key::Enter)); // Focus root (idle -> 在席 prompt, no attach)
    keys.push(Ok(Key::Char(CTRL_O))); // 在席 -> 切替
    keys.push(Ok(Key::ArrowDown)); // root -> main
    keys.push(Ok(Key::ArrowDown)); // main -> feat
    keys.push(Ok(Key::Enter)); // focus feat (live) -> attach
    run_full(
        keys,
        prompt_state(),
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
    .unwrap();
    assert_eq!(
        *opened.borrow(),
        1,
        "Ctrl-O from the prompt surface must reach Switch so focusing the live feat attaches"
    );
}

fn state_with_sessions(names: &[&str]) -> HomeState {
    let mut state = sample_state();
    let sessions = names
        .iter()
        .map(|n| SessionRecord {
            name: n.to_string(),
            display_name: None,
            root: PathBuf::from(format!("/ws/.usagi/sessions/{n}")),
            worktrees: vec![worktree(Some(n), &format!("/ws/{n}"))],
            created_at: Utc::now(),
        })
        .collect();
    state.restore_sessions(sessions);
    state
}

// --- 統括 (Overview) ---------------------------------------------------

#[test]
fn escape_in_overview_is_inert_and_does_not_leave() {
    // Esc no longer backs out to the project list: it is a no-op in Overview,
    // so the loop runs on and only the fallback Ctrl-C (no live session) quits.
    // A Back-returning Esc would instead resolve to `Outcome::Back` here.
    assert!(matches!(
        run(vec![Ok(Key::Escape)], sample_state()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_c_in_overview_returns_quit() {
    assert!(matches!(
        run(vec![Ok(Key::CtrlC)], sample_state()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_o_in_overview_opens_switch() {
    // Ctrl-O zooms into 切替 (Switch) with Overview as the origin; `h` backs
    // out to Overview, where Esc is inert and the fallback Ctrl-C quits.
    let keys = vec![
        Ok(Key::Char(CTRL_O)), // Overview -> Switch
        Ok(Key::Char('h')),    // Switch -> Overview (origin)
        Ok(Key::Escape),       // Esc inert in Overview; fallback Ctrl-C quits
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn text_modal_scrolls_and_dismisses() {
    // `man` opens a scrollable text modal; the arrows / j/k and PageUp/PageDown
    // scroll it, and Esc dismisses it (back to Overview, where the fallback
    // Ctrl-C quits).
    let mut keys = typed("man");
    keys.push(Ok(Key::Enter)); // run `man` -> opens the text modal
    keys.push(Ok(Key::ArrowDown)); // scroll down a line
    keys.push(Ok(Key::Char('j')));
    keys.push(Ok(Key::ArrowUp)); // scroll up a line
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::PageDown)); // page down
    keys.push(Ok(Key::PageUp)); // page up
    keys.push(Ok(Key::Char('z'))); // ignored inside the modal
    keys.push(Ok(Key::Escape)); // dismiss -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn overview_edits_completes_and_recalls_then_runs() {
    let mut keys = typed("ma");
    keys.push(Ok(Key::Backspace));
    keys.push(Ok(Key::Tab)); // "m" -> "man"
    keys.push(Ok(Key::Enter)); // run -> `man` opens its text modal
    keys.push(Ok(Key::Escape)); // dismiss the modal -> Overview
    keys.push(Ok(Key::ArrowUp)); // recall the previous command
    keys.push(Ok(Key::ArrowDown)); // back to empty
    keys.push(Ok(Key::Home)); // ignored
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn overview_caret_keys_edit_within_the_line() {
    // Build "history" by typing out of order and moving the caret with the
    // editing keys, exercising ←/→/End/Del; the recorded command proves the
    // edits landed where the caret was.
    let mut keys = typed("hstory"); // missing the 'i'
    for _ in 0..5 {
        keys.push(Ok(Key::ArrowLeft)); // caret to just after 'h'
    }
    keys.extend(typed("i")); // "history"
    keys.push(Ok(Key::End)); // jump to the end
    keys.extend(typed("X")); // "historyX"
    keys.push(Ok(Key::ArrowLeft)); // caret before the 'X'
    keys.push(Ok(Key::Del)); // delete it -> "history"
    keys.push(Ok(Key::ArrowRight)); // already at the end -> clamped no-op
    keys.push(Ok(Key::Enter)); // run `history` -> opens its text modal
    keys.push(Ok(Key::Escape)); // dismiss the modal
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits

    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut recorded = Vec::new();
    let mut persist = |c: &str| recorded.push(c.to_string());
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(recorded, vec!["history"]);
}

#[test]
fn quit_command_exits_the_app() {
    let mut keys = typed("quit");
    keys.push(Ok(Key::Enter));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn submitted_commands_are_handed_to_persist() {
    let mut keys = typed("man");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut recorded = Vec::new();
    let mut persist = |c: &str| recorded.push(c.to_string());
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(recorded, vec!["man"]);
}

#[test]
fn overview_terminal_and_agent_attach_the_active_session() {
    // Typing `terminal` / `agent` in Overview still dispatches: it focuses the
    // active row (the root) and attaches the pane.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let mut keys = typed("terminal");
    keys.push(Ok(Key::Enter)); // attach (root, plain shell) -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.extend(typed("agent"));
    keys.push(Ok(Key::Enter)); // wait — we are back in Overview after Esc
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
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
    assert_eq!(*opened.borrow(), vec![false, true]);
}

#[test]
fn session_list_logs_the_sessions() {
    let mut keys = typed("session list");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn session_create_with_a_name_creates_immediately() {
    let mut keys = typed("session create newx");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    let created = RefCell::new(Vec::new());
    let mut create = |name: &str| {
        created.borrow_mut().push(name.to_string());
        noop_create(name)
    };
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
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
    assert_eq!(*created.borrow(), vec!["newx"]);
}

#[test]
fn bare_session_create_moves_to_switch_and_opens_the_inline_input() {
    // `session create` (no name) enters 切替 and begins inline creation; the
    // name is typed and confirmed there, creating the session.
    let mut keys = typed("session create");
    keys.push(Ok(Key::Enter)); // -> Switch + begin create
    keys.extend(typed("wip"));
    keys.push(Ok(Key::Enter)); // confirm create -> Focus
    keys.push(Ok(Key::Escape)); // Focus Esc -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    let created = RefCell::new(Vec::new());
    let mut create = |name: &str| {
        created.borrow_mut().push(name.to_string());
        noop_create(name)
    };
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
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
    assert_eq!(*created.borrow(), vec!["wip"]);
}

#[test]
fn session_remove_with_a_name_and_force_routes_to_remove() {
    let mut keys = typed("session remove old --force");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut removed = Vec::new();
    let mut remove = |name: &str, force: bool| {
        removed.push((name.to_string(), force));
        noop_remove(name, force)
    };
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(removed, vec![("old".to_string(), true)]);
}

#[test]
fn close_typed_in_overview_targets_the_active_session() {
    // `close` is a session command, but the Overview line still dispatches it:
    // it force-removes the active session (the root by default). The root is not
    // removable, so `remove` reports no change and the screen stays put.
    let mut keys = typed("close");
    keys.push(Ok(Key::Enter)); // run `close` from the Overview line
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut removed = Vec::new();
    let mut remove = |name: &str, force: bool| {
        removed.push((name.to_string(), force));
        // The root cannot be removed: report no change (no refreshed list).
        noop_remove(name, force)
    };
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(removed, vec![("root".to_string(), true)]);
}

#[test]
fn focus_close_command_force_removes_the_focused_session_then_enters_switch() {
    // 在席 the `feat` session, then run `close` from the prompt: it removes the
    // focused session forcefully (like `session remove feat --force`). Because the
    // focused session is now gone, the screen drops into 切替 (Switch) to pick the
    // next one. We prove the landing mode by pressing `c` — a Switch-only action
    // that opens the inline create input and consults the branch-name callback;
    // in 統括 the same key would just type a character and never call it.
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus (feat)
    keys.extend(typed("close"));
    keys.push(Ok(Key::Enter)); // run `close` -> session removed -> 切替 (Switch)
    keys.push(Ok(Key::Char('c'))); // Switch-only: begin inline create
    keys.push(Ok(Key::Escape)); // cancel create; reader then runs out -> quit
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut removed = Vec::new();
    let mut remove = |name: &str, force: bool| {
        removed.push((name.to_string(), force));
        // Report a refreshed list so the screen leaves 在席 for 切替.
        SessionOutcome {
            line: LogLine::output("removed"),
            sessions: Some(Vec::new()),
            select: None,
        }
    };
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let mut branches_called = 0;
    let mut branches = || {
        branches_called += 1;
        Vec::new()
    };
    let outcome = event_loop(
        &term,
        &mut reader,
        prompt_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(removed, vec![("feat".to_string(), true)]);
    assert_eq!(
        branches_called, 1,
        "`c` after close began inline create, so the screen is in 切替 (Switch)"
    );
}

#[test]
fn focus_menu_close_force_removes_the_focused_session_then_enters_switch() {
    // The 在席 menu lists `close` last; ArrowUp from the top wraps to it. Enter
    // removes the focused session forcefully, then drops into 切替 (Switch) — the
    // `c` keypress that follows opens the inline create input (a Switch-only
    // action), proving the landing mode.
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus (feat), menu UI
    keys.push(Ok(Key::ArrowUp)); // terminal -> wrap to `close`
    keys.push(Ok(Key::Enter)); // run `close` -> session removed -> 切替 (Switch)
    keys.push(Ok(Key::Char('c'))); // Switch-only: begin inline create
    keys.push(Ok(Key::Escape)); // cancel create; reader then runs out -> quit
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut removed = Vec::new();
    let mut remove = |name: &str, force: bool| {
        removed.push((name.to_string(), force));
        SessionOutcome {
            line: LogLine::output("removed"),
            sessions: Some(Vec::new()),
            select: None,
        }
    };
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let mut branches_called = 0;
    let mut branches = || {
        branches_called += 1;
        Vec::new()
    };
    let outcome = event_loop(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(removed, vec![("feat".to_string(), true)]);
    assert_eq!(
        branches_called, 1,
        "`c` after close began inline create, so the screen is in 切替 (Switch)"
    );
}

// --- session-removal modal --------------------------------------------

#[test]
fn session_remove_without_a_name_opens_the_modal_and_bulk_removes() {
    let mut keys = typed("session remove");
    keys.push(Ok(Key::Enter)); // open the modal
    keys.push(Ok(Key::Char(' '))); // check "alpha"
    keys.push(Ok(Key::ArrowDown));
    keys.push(Ok(Key::Char('j'))); // cursor on "gamma"
    keys.push(Ok(Key::Char(' '))); // check "gamma"
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::ArrowUp)); // cursor 0
    keys.push(Ok(Key::Home)); // ignored
    keys.push(Ok(Key::Enter)); // confirm
    keys.push(Ok(Key::Escape)); // Overview back
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut removed = Vec::new();
    let mut remove = |name: &str, force: bool| {
        removed.push((name.to_string(), force));
        noop_remove(name, force)
    };
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop(
        &term,
        &mut reader,
        state_with_sessions(&["alpha", "beta", "gamma"]),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &mut persist,
        &mut create,
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        removed,
        vec![("alpha".to_string(), false), ("gamma".to_string(), false)]
    );
}

#[test]
fn removal_modal_cancels_via_escape_and_keeps_open_on_empty_enter() {
    let mut keys = typed("session remove");
    keys.push(Ok(Key::Enter)); // open
    keys.push(Ok(Key::Enter)); // nothing checked -> stays open
    keys.push(Ok(Key::Char(' '))); // check alpha
    keys.push(Ok(Key::Escape)); // cancel the modal
    keys.push(Ok(Key::Escape)); // Overview back
    assert!(matches!(
        run(keys, state_with_sessions(&["alpha"])).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_c_in_the_removal_modal_quits() {
    let mut keys = typed("session remove");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    assert!(matches!(
        run(keys, state_with_sessions(&["alpha"])).unwrap(),
        Outcome::Quit
    ));
}

// --- quit-confirmation modal (Ctrl-C with a live session) --------------

#[test]
fn ctrl_c_quits_outright_when_no_session_is_live() {
    // The default `run` harness has no live session, so Ctrl-C closes the app
    // without asking — the gate only triggers when something is running.
    assert!(matches!(
        run(vec![Ok(Key::CtrlC)], sample_state()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_c_with_a_live_session_quits_only_after_confirming() {
    // 'y' confirms the close.
    let mut persist: fn(&str) = noop_persist;
    assert!(matches!(
        run_with_live_monitor(
            vec![Ok(Key::CtrlC), Ok(Key::Char('y'))],
            sample_state(),
            &mut persist,
        )
        .unwrap(),
        Outcome::Quit
    ));

    // A second Ctrl-C inside the modal confirms too.
    assert!(matches!(
        run_with_live_monitor(
            vec![Ok(Key::CtrlC), Ok(Key::CtrlC)],
            sample_state(),
            &mut persist,
        )
        .unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn confirm_modal_cancel_keeps_the_screen_running() {
    // Ctrl-C raises the modal (a session is live); an ignored key is a no-op
    // in it; 'n' cancels back to Overview, where a command still runs (proving
    // the first Ctrl-C did not quit). Esc also cancels; Enter finally confirms.
    let mut keys = vec![
        Ok(Key::CtrlC),     // raise the modal
        Ok(Key::Home),      // ignored inside the modal
        Ok(Key::Char('n')), // cancel -> Overview
    ];
    keys.extend(typed("man"));
    keys.push(Ok(Key::Enter)); // runs `man` -> persisted
    keys.push(Ok(Key::CtrlC)); // raise again
    keys.push(Ok(Key::Escape)); // cancel via Esc
    keys.push(Ok(Key::CtrlC)); // raise again
    keys.push(Ok(Key::Enter)); // confirm via Enter -> quit

    let mut recorded = Vec::new();
    let mut persist = |c: &str| recorded.push(c.to_string());
    let outcome = run_with_live_monitor(keys, sample_state(), &mut persist).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // The command ran between the cancelled closes, so the screen kept going.
    assert_eq!(recorded, vec!["man"]);
}

// --- config hand-off ---------------------------------------------------

fn config_keys() -> Vec<io::Result<Key>> {
    let mut keys = typed("config");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    keys
}

#[test]
fn config_opens_the_settings_screen_and_can_quit() {
    // Returns Some -> resume, then back.
    let opened = RefCell::new(false);
    let mut config = |_: &Term| {
        *opened.borrow_mut() = true;
        Ok(Some(SessionActionUi::Menu))
    };
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    assert!(matches!(
        run_full(
            config_keys(),
            sample_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert!(opened.into_inner());

    // Returns None -> quit.
    let mut config_quit = |_: &Term| Ok(None);
    assert!(matches!(
        run_full(
            config_keys(),
            sample_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut config_quit
        )
        .unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn returning_from_config_refreshes_the_session_action_ui() {
    // The config screen flipped the 在席 (Focus) surface from the default Menu to
    // Prompt; on returning to home the state must adopt it, so Focus renders the
    // new surface without reopening the screen. The `terminal` command run right
    // after attaches a pane, letting us observe the live state's setting.
    let mut config = |_: &Term| Ok(Some(SessionActionUi::Prompt));
    let seen = RefCell::new(None);
    let mut open = |state: &mut HomeState, _: &Path, _: bool| {
        *seen.borrow_mut() = Some(state.session_action_ui());
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let mut keys = typed("config");
    keys.push(Ok(Key::Enter));
    keys.extend(typed("terminal"));
    keys.push(Ok(Key::Enter));
    run_full(
        keys,
        sample_state(), // starts as Menu (the default)
        &mut open,
        &mut create,
        &mut preview,
        &mut config,
    )
    .unwrap();
    assert_eq!(seen.into_inner(), Some(SessionActionUi::Prompt));
}

#[test]
fn config_failure_is_propagated() {
    let mut keys = typed("config");
    keys.push(Ok(Key::Enter));
    let mut config = |_: &Term| Err(anyhow::anyhow!("settings blew up"));
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let err = run_full(
        keys,
        sample_state(),
        &mut open,
        &mut create,
        &mut preview,
        &mut config,
    )
    .unwrap_err();
    assert!(err.to_string().contains("settings blew up"));
}

// --- session switch <name> (Overview -> Focus / Attached) --------------

#[test]
fn session_switch_unknown_name_logs_an_error_and_stays_in_overview() {
    let mut keys = typed("session switch nope");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape)); // still in Overview; Esc inert, fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn session_switch_known_idle_name_enters_focus() {
    // "feat" resolves but is idle (no live preview), so it just enters Focus.
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn session_switch_known_live_name_attaches_then_returns_to_focus() {
    // "root" resolves and is live, so it attaches; noop_open closes the pane,
    // returning to Focus, then Esc -> Overview (fallback Ctrl-C quits).
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
    let mut keys = typed("session switch root");
    keys.push(Ok(Key::Enter)); // -> Focus -> attach -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
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
    assert_eq!(*opened.borrow(), 1);
}

// --- 切替 (Switch) -----------------------------------------------------

#[test]
fn switch_navigates_and_backs_out_to_overview() {
    // `session switch` enters Switch; arrows / jk move; Esc returns to Overview
    // (the origin); Esc is then inert, so the fallback Ctrl-C quits.
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch (origin Overview)
    keys.push(Ok(Key::ArrowDown));
    keys.push(Ok(Key::ArrowUp));
    keys.push(Ok(Key::Char('j')));
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::Home)); // ignored
    keys.push(Ok(Key::Char('h'))); // back to Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn switch_ctrl_o_zooms_out_to_overview() {
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::Char(CTRL_O))); // -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn switch_snapshots_the_highlighted_live_session_for_the_preview() {
    // In 切替 the render loop snapshots the highlighted session's live
    // terminal so the right pane previews the actual screen. Under the live
    // harness `preview` returns a snapshot, exercising that path.
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::ArrowDown)); // move onto a live session row
    keys.push(Ok(Key::Char(CTRL_O))); // -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    assert!(matches!(
        run_live(keys, sample_state()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn switch_ctrl_c_quits() {
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn switch_enter_on_an_idle_session_just_focuses_it() {
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::ArrowDown)); // cursor on "main"
    keys.push(Ok(Key::Enter)); // focus (idle -> no attach)
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn switch_enter_on_a_live_session_attaches_via_l() {
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::Char('l'))); // focus + attach (live)
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
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
    assert_eq!(*opened.borrow(), 1);
}

#[test]
fn switch_inline_create_makes_and_focuses_the_new_session() {
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::Char('c'))); // begin create
    keys.push(Ok(Key::Insert)); // unhandled inside create: the `_` arm
    keys.extend(typed("Xwip")); // a stray leading 'X' to edit out
    keys.push(Ok(Key::Home)); // caret to the start
    keys.push(Ok(Key::Del)); // forward-delete the 'X' -> "wip"
    keys.push(Ok(Key::End)); // caret to the end
    keys.push(Ok(Key::ArrowLeft)); // caret before 'p'
    keys.push(Ok(Key::ArrowRight)); // caret after 'p' (end)
    keys.push(Ok(Key::Backspace)); // "wi"
    keys.push(Ok(Key::Char('p'))); // "wip"
    keys.push(Ok(Key::Enter)); // confirm -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    let created = RefCell::new(Vec::new());
    let mut create = |name: &str| {
        created.borrow_mut().push(name.to_string());
        noop_create(name)
    };
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
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
    assert_eq!(*created.borrow(), vec!["wip"]);
}

#[test]
fn switch_inline_create_can_be_cancelled_and_ctrl_c_quits() {
    // Cancel path: Esc closes the input, staying in Switch; then Ctrl-O -> Overview (fallback Ctrl-C quits).
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Char('c'))); // begin create
    keys.push(Ok(Key::Char('x')));
    keys.push(Ok(Key::Escape)); // cancel create (stay in Switch)
    keys.push(Ok(Key::Char(CTRL_O))); // Switch -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));

    // Ctrl-C inside the create input quits.
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Char('c')));
    keys.push(Ok(Key::CtrlC));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn switch_create_invalid_name_keeps_the_input_open() {
    // An empty confirm keeps the input open; then Ctrl-C ends the run.
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Char('c')));
    keys.push(Ok(Key::Enter)); // empty -> error, stays open
    keys.push(Ok(Key::CtrlC));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
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
    let mut open: fn(&mut HomeState, &Path, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<SessionActionUi>> = noop_config;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &UpdateHandle::new(),
        &mut persist,
        &mut create,
        &mut rename,
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
    )
    .unwrap();
    (renamed.into_inner(), outcome)
}

#[test]
fn switch_inline_rename_edits_then_confirms_the_label() {
    // Switch -> cursor onto "main" -> `r` (prefills "main") -> clear -> type
    // "Top" -> Enter persists via the rename callback.
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::ArrowDown)); // cursor "main"
    keys.push(Ok(Key::Char('r'))); // begin rename (prefilled "main")
    keys.push(Ok(Key::ArrowUp)); // a non-edit key is ignored while renaming
    for _ in 0..4 {
        keys.push(Ok(Key::Backspace)); // clear the prefill
    }
    keys.extend(typed("Top"));
    keys.push(Ok(Key::Enter)); // confirm -> rename callback
    keys.push(Ok(Key::CtrlC)); // quit
    let (renamed, outcome) = run_recording_rename(keys);
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(renamed, vec![("main".to_string(), "Top".to_string())]);
}

#[test]
fn switch_inline_rename_can_be_cancelled_with_no_persist() {
    // `r` opens the input, Esc closes it without calling the rename callback.
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::ArrowDown)); // cursor "main"
    keys.push(Ok(Key::Char('r'))); // begin rename
    keys.push(Ok(Key::Char('x'))); // type something
    keys.push(Ok(Key::Escape)); // cancel (stay in Switch)
    keys.push(Ok(Key::CtrlC));
    let (renamed, outcome) = run_recording_rename(keys);
    assert!(matches!(outcome, Outcome::Quit));
    assert!(renamed.is_empty());
}

#[test]
fn switch_rename_on_the_root_row_is_a_noop() {
    // `r` on the root row (no session) opens nothing; the run just quits.
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch (cursor on root)
    keys.push(Ok(Key::Char('r'))); // no-op on root
    keys.push(Ok(Key::CtrlC));
    let (renamed, outcome) = run_recording_rename(keys);
    assert!(matches!(outcome, Outcome::Quit));
    assert!(renamed.is_empty());
}

// --- 在席 (Focus) menu surface -----------------------------------------

#[test]
fn focus_menu_moves_and_runs_terminal_via_enter() {
    // Switch -> focus "main" (idle, so just Focus). The menu highlights
    // "terminal" by default; move down to "agent" and back up to "terminal",
    // then Enter runs it (attaches).
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, a: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // Switch
    keys.push(Ok(Key::ArrowDown)); // cursor "main" (/r/main)
    keys.push(Ok(Key::Enter)); // focus main (idle)
    keys.push(Ok(Key::Char('j'))); // terminal -> agent
    keys.push(Ok(Key::ArrowUp)); // agent -> terminal
    keys.push(Ok(Key::Enter)); // run terminal (attach) -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
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
    assert_eq!(*opened.borrow(), vec![(PathBuf::from("/r/main"), false)]);
}

#[test]
fn focus_menu_shortcut_keys_launch_terminal_and_agent() {
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char('t'))); // terminal
    keys.push(Ok(Key::Char('k'))); // a menu move (no-op effect here)
    keys.push(Ok(Key::Char('a'))); // agent
    keys.push(Ok(Key::Escape)); // -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
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
    assert_eq!(*opened.borrow(), vec![false, true]);
}

#[test]
fn focus_menu_can_run_the_coming_soon_ai_command() {
    // The menu lists terminal (0, default), agent (1), ai (2), close (3).
    // ArrowUp from the top wraps to "close"; one more lands on "ai"; Enter on
    // it just logs (no attach).
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus
    keys.push(Ok(Key::Home)); // ignored in the menu
    keys.push(Ok(Key::ArrowDown)); // terminal -> agent
    keys.push(Ok(Key::ArrowUp)); // back to terminal
    keys.push(Ok(Key::ArrowUp)); // wrap to "close"
    keys.push(Ok(Key::ArrowUp)); // up to "ai"
    keys.push(Ok(Key::Enter)); // run ai (coming soon)
    keys.push(Ok(Key::Escape)); // -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_o_opens_switch_then_esc_re_focuses() {
    // Focus -> Ctrl-O -> Switch(return=Focus); Esc/h re-enters Focus; Esc ->
    // Overview; Esc inert, fallback Ctrl-C quits.
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // -> Switch(return Focus)
    keys.push(Ok(Key::Char('h'))); // back -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_c_quits() {
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

// --- 在席 (Focus) prompt surface ---------------------------------------

#[test]
fn focus_prompt_edits_completes_and_runs_terminal() {
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool| {
        assert!(!a);
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat (prompt UI)
    keys.extend(typed("ter"));
    keys.push(Ok(Key::Insert)); // unhandled in the prompt: the `_` arm
    keys.push(Ok(Key::Home)); // caret to the start
    keys.push(Ok(Key::End)); // caret to the end
    keys.push(Ok(Key::ArrowLeft)); // caret before 'r'
    keys.push(Ok(Key::Del)); // forward-delete 'r' -> "te"
    keys.push(Ok(Key::Char('r'))); // "ter" again, caret at end
    keys.push(Ok(Key::ArrowLeft)); // before 'r'
    keys.push(Ok(Key::ArrowRight)); // after 'r' (end)
    keys.push(Ok(Key::Backspace)); // "te"
    keys.push(Ok(Key::Tab)); // -> "terminal"
    keys.push(Ok(Key::Enter)); // run terminal (attach)
    keys.push(Ok(Key::Escape)); // -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            prompt_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), 1);
}

#[test]
fn focus_prompt_runs_agent_and_coming_soon_and_ignores_empty() {
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = noop_preview;
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus (prompt)
    keys.push(Ok(Key::Home)); // ignored in the prompt
    keys.push(Ok(Key::Enter)); // empty prompt -> no-op
    keys.extend(typed("ai go"));
    keys.push(Ok(Key::Enter)); // coming soon -> log, no attach
    keys.extend(typed("agent"));
    keys.push(Ok(Key::Enter)); // attach agent
    keys.push(Ok(Key::Escape)); // -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            prompt_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![true]);
}

// --- 没入 (Attached) exits ---------------------------------------------

#[test]
fn ctrl_o_in_the_pane_zooms_out_to_switch() {
    // Attaching to a live session; the pane returns ToSwitch (Ctrl-O), so the
    // loop enters Switch with return=Attached. Then Ctrl-O -> Overview (fallback Ctrl-C quits).
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| Ok(PaneExit::ToSwitch);
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
    let mut keys = typed("session switch root");
    keys.push(Ok(Key::Enter)); // Focus root -> attach -> ToSwitch -> Switch
    keys.push(Ok(Key::Char(CTRL_O))); // Switch -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
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
}

#[test]
fn pane_to_switch_then_esc_re_attaches() {
    // ToSwitch -> Switch(return=Attached). In Switch, Esc re-attaches. The pane
    // returns ToSwitch the first time and Closed the second so the run ends.
    let calls = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| {
        let mut n = calls.borrow_mut();
        *n += 1;
        if *n == 1 {
            Ok(PaneExit::ToSwitch)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
    let mut keys = typed("session switch root");
    keys.push(Ok(Key::Enter)); // attach -> ToSwitch -> Switch(return Attached)
    keys.push(Ok(Key::Escape)); // Switch Esc -> re-attach -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
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
    assert_eq!(*calls.borrow(), 2);
}

#[test]
fn pane_to_switch_then_esc_onto_an_idle_session_lands_in_focus() {
    // ToSwitch -> Switch(return=Attached). Moving the cursor onto an idle
    // session and pressing Esc lands in 在席 *without* spawning a second pane
    // — only a live session re-attaches, mirroring how Enter behaves.
    let calls = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| {
        *calls.borrow_mut() += 1;
        Ok(PaneExit::ToSwitch)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    // Only the root (/ws) is live; the worktree rows are idle.
    let mut preview = |p: &Path| {
        if p == Path::new("/ws") {
            Some(TerminalView::from_rows(vec!["live".to_string()], None))
        } else {
            None
        }
    };
    let mut keys = typed("session switch root");
    keys.push(Ok(Key::Enter)); // attach root -> ToSwitch -> Switch(return Attached)
    keys.push(Ok(Key::ArrowDown)); // cursor -> an idle worktree row
    keys.push(Ok(Key::Escape)); // Esc -> idle row stays in Focus (no re-attach)
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
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
    // The pane opened only once (the initial attach); the Esc did not re-attach.
    assert_eq!(*calls.borrow(), 1);
}

#[test]
fn pane_failure_is_reported_and_returns_to_focus() {
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool| Err(anyhow::anyhow!("no shell"));
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path) -> Option<TerminalView> = live_preview;
    let mut keys = typed("session switch root");
    keys.push(Ok(Key::Enter)); // attach -> Err -> Focus (logged)
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
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
}

// --- read errors -------------------------------------------------------

#[test]
fn interrupted_read_returns_quit() {
    let keys = vec![Err(io::Error::new(
        io::ErrorKind::Interrupted,
        "interrupted",
    ))];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn unexpected_read_error_is_propagated() {
    let err = run(vec![Err(io::Error::other("boom"))], sample_state()).unwrap_err();
    assert!(err.to_string().contains("Failed to read key"));
}

#[test]
fn page_keys_are_inert_in_overview() {
    let keys = vec![Ok(Key::PageUp), Ok(Key::PageDown), Ok(Key::Escape)];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn default_callbacks_run_through_the_harness() {
    // Drive the shared no-op `open_terminal` (via the live harness, which
    // attaches) and `open_config` (via `config`) so both default callbacks
    // execute end to end.
    let mut keys = typed("session switch root");
    keys.push(Ok(Key::Enter)); // live -> attach via noop_open -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    assert!(matches!(
        run_live(keys, sample_state()).unwrap(),
        Outcome::Quit
    ));

    // `config` through the default `noop_config` (returns false -> resume).
    let mut keys = typed("config");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}
