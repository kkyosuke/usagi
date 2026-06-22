use super::super::oneshot::OneShot;
use super::super::state::LogLine;
use super::super::terminal_tabs::TabNav;
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
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        sample_state(),
        Path::new("/ws"),
        &monitor,
        &update,
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
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    // Only `feat` has a live terminal, so focusing the idle root stays in 在席
    // (no auto-attach) until Ctrl-O reaches Switch and `feat` is selected.
    let mut preview = |p: &Path, _: Sidebar| {
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
            note: None,
            root: PathBuf::from(format!("/ws/.usagi/sessions/{n}")),
            worktrees: vec![worktree(Some(n), &format!("/ws/{n}"))],
            created_at: Utc::now(),
        })
        .collect();
    state.restore_sessions(sessions);
    state
}

#[test]
fn a_background_refresh_updates_the_session_list_exactly_once() {
    // The pane-exit sync thread publishes a freshly-synced list to the handle;
    // the loop's `apply_pending_refresh` adopts it on a later frame. With nothing
    // pending the state is untouched; once a list lands it is applied and then
    // taken, so a second poll does not re-apply a stale snapshot. The return tells
    // the loop whether to force a repaint (a landed list changes the git statuses).
    let mut state = state_with_sessions(&["main", "feat"]);
    let refresh = SessionsRefreshHandle::new();

    // No sync has landed yet: the list is left exactly as it was, and the loop is
    // told nothing changed.
    assert!(!apply_pending_refresh(&mut state, &refresh));
    assert_eq!(
        state
            .sessions()
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>(),
        vec!["main".to_string(), "feat".to_string()]
    );

    // A background sync reports that `feat` is gone and `next` was added.
    refresh.set(
        ["main", "next"]
            .iter()
            .map(|n| SessionRecord {
                name: n.to_string(),
                display_name: None,
                note: None,
                root: PathBuf::from(format!("/ws/.usagi/sessions/{n}")),
                worktrees: vec![worktree(Some(n), &format!("/ws/{n}"))],
                created_at: Utc::now(),
            })
            .collect(),
    );
    assert!(apply_pending_refresh(&mut state, &refresh));
    assert_eq!(
        state
            .sessions()
            .iter()
            .map(|s| s.name.clone())
            .collect::<Vec<_>>(),
        vec!["main".to_string(), "next".to_string()]
    );

    // The slot is now empty, so a further poll re-applies nothing.
    refresh.set(Vec::new());
    assert!(apply_pending_refresh(&mut state, &refresh));
    assert!(state.sessions().is_empty());
    assert!(!apply_pending_refresh(&mut state, &refresh));
    assert!(state.sessions().is_empty());
}

// --- background startup results (the local-LLM probe one-shot) ---------

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
    )
}

#[test]
fn the_background_llm_probe_result_is_drained_then_the_loop_quits() {
    // The local-LLM probe confirms availability through the one-shot; the first
    // frame drains it (flipping the `ai` command on via `set_ai_available`), then
    // Ctrl-C with nothing live quits.
    let ai = OneShot::<bool>::new();
    ai.set(true);
    assert!(matches!(
        run_with_ai_probe(vec![Ok(Key::CtrlC)], sample_state(), &ai).unwrap(),
        Outcome::Quit
    ));
    assert!(ai.take().is_none());
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
fn ctrl_b_toggles_the_sidebar_and_keeps_the_screen_running() {
    // Ctrl-B is a view-only sidebar toggle handled before the per-mode dispatch:
    // the loop collapses / expands the sidebar and keeps running, so the
    // following Ctrl-C still quits. Two presses exercise both directions.
    let keys = vec![
        Ok(Key::Char(CTRL_B)), // Full -> Rail
        Ok(Key::Char(CTRL_B)), // Rail -> Full
        Ok(Key::CtrlC),        // still running -> quit
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn ctrl_o_in_overview_opens_switch() {
    // Ctrl-O zooms into 切替 (Switch) with Overview as the origin; `Esc` backs
    // out to Overview, where Esc is then inert and the fallback Ctrl-C quits.
    let keys = vec![
        Ok(Key::Char(CTRL_O)), // Overview -> Switch
        Ok(Key::Escape),       // Switch -> Overview (origin)
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
fn preview_command_opens_reads_scrolls_and_dismisses_the_markdown_pane() {
    // `preview <file>` resolves and reads the file under the workspace root, opens
    // the right-pane preview, and then the arrows / j/k and PageUp/PageDown scroll
    // it while Esc dismisses it (back to Overview, where Ctrl-C quits).
    let dir = tempfile::tempdir().unwrap();
    let body = (0..40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.path().join("README.md"), format!("# Title\n{body}")).unwrap();

    let mut keys = typed("preview README");
    keys.push(Ok(Key::Enter)); // run `preview` -> reads the file, opens the pane
    keys.push(Ok(Key::ArrowDown)); // scroll down a line
    keys.push(Ok(Key::Char('j')));
    keys.push(Ok(Key::ArrowUp)); // scroll up a line
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::PageDown)); // page down
    keys.push(Ok(Key::PageUp)); // page up
    keys.push(Ok(Key::Char('z'))); // ignored inside the preview
    keys.push(Ok(Key::Escape)); // dismiss -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert in Overview; fallback Ctrl-C quits
    assert!(matches!(
        run_at(keys, sample_state(), dir.path()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn preview_command_logs_a_failure_for_a_missing_file() {
    // A `preview` of a file that does not exist opens nothing and logs the error;
    // the screen keeps running and quits on the trailing Ctrl-C.
    let dir = tempfile::tempdir().unwrap();
    let mut keys = typed("preview missing");
    keys.push(Ok(Key::Enter)); // run `preview` -> read fails, nothing opens
    keys.push(Ok(Key::Escape)); // Esc inert in Overview (no preview captured it)
    assert!(matches!(
        run_at(keys, sample_state(), dir.path()).unwrap(),
        Outcome::Quit
    ));
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
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
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
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
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
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
fn session_list_with_sessions_opens_a_modal() {
    // With sessions recorded, `session list` opens the scrollable Sessions modal
    // (the empty-state path is a one-liner); Esc then dismisses it.
    let mut keys = typed("session list");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    assert!(matches!(
        run(keys, state_with_sessions(&["alpha", "beta"])).unwrap(),
        Outcome::Quit
    ));
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
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(removed, vec![("old".to_string(), true)]);
}

#[test]
fn close_typed_in_overview_on_root_is_refused() {
    // `close` is a session command, and the Overview line still dispatches it.
    // The focused row is the root by default, which is the workspace itself and
    // not a session, so `close` is refused outright: `remove` is never called and
    // the screen stays put.
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
        noop_remove(name, force)
    };
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
        &mut (noop_rename as fn(&str, &str) -> SessionOutcome),
        &mut (noop_set_note as fn(&str, &str) -> SessionOutcome),
        &mut remove,
        &mut (no_branches as fn() -> Vec<String>),
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(
        removed.is_empty(),
        "close on the root row must not call remove"
    );
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
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches_called = 0;
    let mut branches = || {
        branches_called += 1;
        Vec::new()
    };
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        prompt_state(),
        Path::new("/ws"),
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
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches_called = 0;
    let mut branches = || {
        branches_called += 1;
        Vec::new()
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
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut (noop_close as fn(&mut HomeState, &Path)),
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
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let outcome = event_loop_compat(
        &term,
        &mut reader,
        state_with_sessions(&["alpha", "beta", "gamma"]),
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

// --- interrupted reads (EINTR) must not quit ---------------------------

#[test]
fn an_interrupted_blocking_read_is_retried_not_treated_as_quit() {
    // With no live session the loop blocks in `read_key`. A delivered signal
    // (`EINTR`) interrupting that read must not quit: before the fix the loop
    // returned `Outcome::Quit`, which dropped the user out of the alternate
    // screen and exposed the pre-launch scrollback. It now re-reads, so the
    // typed command still runs before the trailing Ctrl-C quits.
    let created = RefCell::new(Vec::<String>::new());
    let mut create = |name: &str| {
        created.borrow_mut().push(name.to_string());
        noop_create(name)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys: Vec<io::Result<Key>> = vec![Err(io::Error::from(io::ErrorKind::Interrupted))];
    keys.extend(typed("session create foo"));
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    let outcome = run_full(
        keys,
        sample_state(),
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*created.borrow(), vec!["foo".to_string()]);
}

#[test]
fn an_interrupted_animate_read_is_retried_not_treated_as_quit() {
    // While a session is live the loop waits in `read_key_timeout` (the animate
    // path) — the exact case the user hit: exit an agent, then `Ctrl-O` while
    // waiting, and a signal interrupting the wait quit the app. The interrupted
    // read is now swallowed, the typed command still runs, and only the
    // confirmed Ctrl-C quits.
    let mut keys: Vec<io::Result<Key>> = vec![Err(io::Error::from(io::ErrorKind::Interrupted))];
    keys.extend(typed("man"));
    keys.push(Ok(Key::Enter)); // runs `man` -> persisted
    keys.push(Ok(Key::CtrlC)); // raise the quit modal (a session is live)
    keys.push(Ok(Key::Enter)); // confirm -> quit
    let mut recorded = Vec::new();
    let mut persist = |c: &str| recorded.push(c.to_string());
    let outcome = run_with_live_monitor(keys, sample_state(), &mut persist).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // `man` ran after the interrupted read, proving it was not treated as quit.
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
        Ok(Some(reload(SessionActionUi::Menu)))
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
    let mut config = |_: &Term| Ok(Some(reload(SessionActionUi::Prompt)));
    let seen = RefCell::new(None);
    let mut open = |state: &mut HomeState, _: &Path, _: bool, _: bool| {
        *seen.borrow_mut() = Some(state.session_action_ui());
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
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
    // `session switch` enters Switch; ↑/↓ (jk) move between sessions and ←/→ (hl)
    // between the highlighted session's tabs (a no-op with no panes here); Esc
    // returns to Overview (the origin); Esc is then inert, so the fallback Ctrl-C
    // quits.
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch (origin Overview)
    keys.push(Ok(Key::ArrowDown));
    keys.push(Ok(Key::ArrowUp));
    keys.push(Ok(Key::Char('j')));
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::ArrowLeft)); // tab prev (no panes -> no-op)
    keys.push(Ok(Key::ArrowRight)); // tab next (no-op)
    keys.push(Ok(Key::Char('h'))); // tab prev via vim key (no-op)
    keys.push(Ok(Key::Char('l'))); // tab next via vim key (no-op)
    keys.push(Ok(Key::Home)); // ignored
    keys.push(Ok(Key::Escape)); // back to Overview
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
fn switch_enter_on_a_live_session_re_attaches_its_active_pane() {
    // Enter on a live session re-attaches (no new pane), so `open_terminal` is
    // called once with `new_pane == false`.
    let opened = RefCell::new(0);
    let new_pane_seen = RefCell::new(None);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, n: bool| {
        *opened.borrow_mut() += 1;
        *new_pane_seen.borrow_mut() = Some(n);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::Enter)); // focus + attach (live)
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
    assert_eq!(*new_pane_seen.borrow(), Some(false));
}

#[test]
fn switch_t_opens_the_action_surface_and_adds_a_new_pane() {
    // `t` in 切替 opens the selected session's action surface (在席) instead of
    // attaching; running `terminal` there adds a *new* pane, so `open_terminal`
    // is called with new_pane == true.
    let opened = RefCell::new(0);
    let new_pane_seen = RefCell::new(None);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, n: bool| {
        *opened.borrow_mut() += 1;
        *new_pane_seen.borrow_mut() = Some(n);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::Char('t'))); // -> 在席 action surface (Menu)
    keys.push(Ok(Key::Char('t'))); // menu: run terminal -> adds a new pane
    keys.push(Ok(Key::Escape)); // 在席 -> Overview
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
    assert_eq!(*new_pane_seen.borrow(), Some(true));
}

#[test]
fn switch_arrows_move_the_active_tab_via_tab_op() {
    // ←/→ (and the vim h/l, and Ctrl-N/Ctrl-P) drive `tab_op` with a `TabNav`,
    // moving the highlighted session's active tab without leaving 切替.
    let term = Term::stdout();
    let navs = RefCell::new(Vec::new());
    let mut tab_op = |_d: &Path, nav: Option<TabNav>| -> (Vec<String>, usize) {
        if let Some(n) = nav {
            navs.borrow_mut().push(n);
        }
        (vec!["agent".to_string(), "terminal".to_string()], 0)
    };
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::ArrowRight)); // tab next
    keys.push(Ok(Key::ArrowLeft)); // tab prev
    keys.push(Ok(Key::Char('l'))); // tab next (vim)
    keys.push(Ok(Key::Char('h'))); // tab prev (vim)
    keys.push(Ok(Key::Char(CTRL_N))); // tab next (Ctrl-N)
    keys.push(Ok(Key::Char(CTRL_P))); // tab prev (Ctrl-P)
    keys.push(Ok(Key::CtrlC)); // quit
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
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
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *navs.borrow(),
        vec![
            TabNav::Next,
            TabNav::Prev,
            TabNav::Next,
            TabNav::Prev,
            TabNav::Next,
            TabNav::Prev,
        ]
    );
}

#[test]
fn switch_x_closes_the_highlighted_sessions_active_tab() {
    // `x` in 切替 drives `close_tab` with the highlighted session's path, closing
    // its active tab (pane) without leaving the picker.
    let term = Term::stdout();
    let closed = RefCell::new(Vec::new());
    let mut close_tab = |_h: &mut HomeState, dir: &Path| {
        closed.borrow_mut().push(dir.to_path_buf());
    };
    let mut keys = typed("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch (cursor on the root row)
    keys.push(Ok(Key::ArrowDown)); // -> the first session (main, /r/main)
    keys.push(Ok(Key::Char('x'))); // close its active tab
    keys.push(Ok(Key::CtrlC)); // quit
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
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
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut (noop_tab_op as fn(&Path, Option<TabNav>) -> (Vec<String>, usize)),
        &mut close_tab,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*closed.borrow(), vec![PathBuf::from("/r/main")]);
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
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
    let mut open = |_h: &mut HomeState, d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((d.to_path_buf(), a));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
    // With the local LLM available the menu lists terminal (0, default),
    // agent (1), ai (2), close (3). ArrowUp from the top wraps to "close"; one
    // more lands on "ai"; Enter on it just logs (no attach).
    let mut state = sample_state();
    state.set_ai_available(true);
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
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_o_opens_switch_then_esc_re_focuses() {
    // Focus -> Ctrl-O -> Switch(return=Focus); Esc re-enters Focus; Esc ->
    // Overview; Esc inert, fallback Ctrl-C quits.
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // -> Switch(return Focus)
    keys.push(Ok(Key::Escape)); // back -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_n_and_ctrl_p_move_the_active_tab_via_tab_op() {
    // In 在席, Ctrl-N / Ctrl-P move the focused session's active tab through
    // `tab_op` (so a later re-attach / `terminal` lands on the chosen pane),
    // mirroring 切替 and 没入 — and they stay in Focus.
    let term = Term::stdout();
    let navs = RefCell::new(Vec::new());
    let mut tab_op = |_d: &Path, nav: Option<TabNav>| -> (Vec<String>, usize) {
        if let Some(n) = nav {
            navs.borrow_mut().push(n);
        }
        (vec!["agent".to_string(), "terminal".to_string()], 0)
    };
    let mut keys = typed("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus feat
    keys.push(Ok(Key::Char(CTRL_N))); // tab next
    keys.push(Ok(Key::Char(CTRL_P))); // tab prev
    keys.push(Ok(Key::CtrlC)); // quit
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches: fn() -> Vec<String> = no_branches;
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
        &mut branches,
        &mut open,
        &mut config,
        &mut preview,
        &mut tab_op,
        &mut (noop_close as fn(&mut HomeState, &Path)),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*navs.borrow(), vec![TabNav::Next, TabNav::Prev]);
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
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        assert!(!a);
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
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
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToSwitch);
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
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
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        let mut n = calls.borrow_mut();
        *n += 1;
        if *n == 1 {
            Ok(PaneExit::ToSwitch)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
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
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *calls.borrow_mut() += 1;
        Ok(PaneExit::ToSwitch)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    // Only the root (/ws) is live; the worktree rows are idle.
    let mut preview = |p: &Path, _: Sidebar| {
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
    let mut open =
        |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Err(anyhow::anyhow!("no shell"));
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
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

// --- background-task read & drain --------------------------------------

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
    let mut wiring = Wiring {
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        dispatch_remove: &mut dispatch_remove,
        evict_pool: &mut evict_pool,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        open_config: &mut config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
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

#[test]
fn a_tick_with_no_key_re_iterates_while_a_task_runs() {
    // A running task keeps the loop animating: the read wakes on the timeout
    // with no key (Ok(None)), the loop re-iterates and repaints, then the next
    // timeout yields Ctrl-C and the idle screen quits.
    let tasks = TaskHandle::new();
    tasks.begin(super::super::tasks::TaskKind::CreateSession, "x");
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Ok(None), Ok(Some(Key::CtrlC))]),
        blocking: VecDeque::new(),
    };
    let mut remove = |_: &str, _: bool| {};
    let mut evict = |_: &Path| {};
    assert!(matches!(
        run_with_tasks(&tasks, &mut reader, &mut remove, &mut evict).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn an_interrupted_timeout_read_returns_quit() {
    // While a task animates, an interrupted timeout read means quit.
    let tasks = TaskHandle::new();
    tasks.begin(super::super::tasks::TaskKind::CreateSession, "x");
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "interrupted",
        ))]),
        blocking: VecDeque::new(),
    };
    let mut remove = |_: &str, _: bool| {};
    let mut evict = |_: &Path| {};
    assert!(matches!(
        run_with_tasks(&tasks, &mut reader, &mut remove, &mut evict).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn an_unexpected_timeout_read_error_is_propagated() {
    let tasks = TaskHandle::new();
    tasks.begin(super::super::tasks::TaskKind::CreateSession, "x");
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Err(io::Error::other("boom"))]),
        blocking: VecDeque::new(),
    };
    let mut remove = |_: &str, _: bool| {};
    let mut evict = |_: &Path| {};
    let err = run_with_tasks(&tasks, &mut reader, &mut remove, &mut evict).unwrap_err();
    assert!(err.to_string().contains("Failed to read key"));
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
    let mut wiring = Wiring {
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        dispatch_remove: &mut dispatch_remove,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        open_config: &mut config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
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

#[test]
fn a_live_session_wakes_the_loop_without_a_key() {
    // Regression for #66: with no install or task running but a session live, the
    // loop must still wake on the timeout tick so a background agent's badge
    // (waiting ◆ / finished ✓) and the update notice are reflected without the
    // user pressing a key. The first tick yields no key (Ok(None)) and the loop
    // re-iterates; the live session makes the next Ctrl-C raise the quit-confirm
    // modal, and a second confirms. The blocking queue holds an error: were the
    // loop to block on `read_key` (the bug), it would surface here instead of
    // quitting cleanly through the timeout path.
    let mut reader = TimeoutScript {
        timeouts: VecDeque::from(vec![Ok(None), Ok(Some(Key::CtrlC)), Ok(Some(Key::CtrlC))]),
        blocking: VecDeque::from(vec![Err(io::Error::other("loop blocked on a key"))]),
    };
    assert!(matches!(
        run_with_live_session(&mut reader).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn a_finished_removal_evicts_the_pooled_shell() {
    // A completed removal carrying an evict path makes the loop evict that pool
    // path on the next drain (on this thread, since the pool is not `Send`).
    let tasks = TaskHandle::new();
    let id = tasks.begin(super::super::tasks::TaskKind::RemoveSession, "feat");
    let path = PathBuf::from("/ws/.usagi/sessions/feat");
    tasks.complete(
        id,
        true,
        super::super::tasks::Completion {
            line: LogLine::output("Removed session \"feat\" 🧹"),
            sessions: None,
            evict: Some(path.clone()),
        },
    );
    let mut reader = ScriptedReader::new(vec![Ok(Key::CtrlC)]);
    let mut remove = |_: &str, _: bool| {};
    let evicted = RefCell::new(Vec::new());
    let mut evict = |p: &Path| evicted.borrow_mut().push(p.to_path_buf());
    assert!(matches!(
        run_with_tasks(&tasks, &mut reader, &mut remove, &mut evict).unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*evicted.borrow(), vec![path]);
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

// --- session note editor ----------------------------------------------

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
    )
}

#[test]
fn switch_n_opens_the_note_editor_edits_the_buffer_and_saves() {
    // 切替, `n` on a session opens the editor; the editing keys build a
    // multi-line note, and Ctrl-S persists it through `set_note`.
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;

    let mut keys = vec![
        Ok(Key::Char(CTRL_O)),     // Overview -> Switch (cursor on root)
        Ok(Key::Char('n')),        // `n` on the root row: a no-op (not a session)
        Ok(Key::ArrowDown),        // root -> alpha
        Ok(Key::Char('n')),        // open the note editor for alpha
        Ok(Key::Tab),              // ignored inside the editor
        Ok(Key::Char('\u{0001}')), // a control char (Ctrl-A): ignored
    ];
    keys.extend(typed("abc"));
    keys.push(Ok(Key::ArrowLeft));
    keys.push(Ok(Key::ArrowRight));
    keys.push(Ok(Key::Home));
    keys.push(Ok(Key::End));
    keys.push(Ok(Key::Backspace)); // "abc" -> "ab"
    keys.push(Ok(Key::Del)); // at end of buffer: no-op
    keys.push(Ok(Key::Enter)); // "ab" -> "ab\n"
    keys.push(Ok(Key::Char('z'))); // "ab\nz"
    keys.push(Ok(Key::ArrowUp));
    keys.push(Ok(Key::ArrowDown));
    keys.push(Ok(Key::Char(CTRL_S))); // save
    keys.push(Ok(Key::Escape)); // Switch -> Overview
    keys.push(Ok(Key::CtrlC)); // quit

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha", "beta"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *recorded.borrow(),
        vec![("alpha".to_string(), "ab\nz".to_string())]
    );
}

#[test]
fn switch_n_note_editor_cancel_discards_the_edit() {
    // Esc closes the editor without persisting anything.
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;

    let mut keys = vec![
        Ok(Key::Char(CTRL_O)), // Overview -> Switch
        Ok(Key::ArrowDown),    // root -> alpha
        Ok(Key::Char('n')),    // open the editor
    ];
    keys.extend(typed("draft"));
    keys.push(Ok(Key::Escape)); // cancel the editor (no save)
    keys.push(Ok(Key::Escape)); // Switch -> Overview
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(recorded.borrow().is_empty(), "cancel must not save");
}

#[test]
fn attached_ctrl_e_opens_the_note_editor_then_re_attaches_on_save() {
    // Attaching a live session, then `Ctrl-E` (reported as PaneExit::OpenNote)
    // opens the note editor over the pane; saving persists the note and
    // re-attaches (open_terminal is driven a second time).
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        let mut n = opened.borrow_mut();
        *n += 1;
        // First attach yields to the note editor; the re-attach then closes.
        if *n == 1 {
            Ok(PaneExit::OpenNote)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = typed("session switch alpha");
    keys.push(Ok(Key::Enter)); // focus + attach alpha -> open_terminal #1 -> OpenNote
    keys.extend(typed("hi")); // edit the note in the editor
    keys.push(Ok(Key::Char(CTRL_S))); // save -> re-attach -> open_terminal #2 -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *opened.borrow(),
        2,
        "the pane is re-attached after the editor"
    );
    assert_eq!(
        *recorded.borrow(),
        vec![("alpha".to_string(), "hi".to_string())]
    );
}

#[test]
fn attached_ctrl_e_re_attaches_on_cancel_without_saving() {
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        let mut n = opened.borrow_mut();
        *n += 1;
        if *n == 1 {
            Ok(PaneExit::OpenNote)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = typed("session switch alpha");
    keys.push(Ok(Key::Enter)); // attach -> OpenNote
    keys.extend(typed("scratch"));
    keys.push(Ok(Key::Escape)); // cancel -> re-attach -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Overview
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*opened.borrow(), 2, "cancel still re-attaches the pane");
    assert!(recorded.borrow().is_empty(), "cancel must not save");
}

#[test]
fn attached_ctrl_e_on_the_root_row_re_attaches_without_opening_an_editor() {
    // The root row is the workspace, not a session: Ctrl-E there opens no editor
    // and simply re-attaches the pane.
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        let mut n = opened.borrow_mut();
        *n += 1;
        if *n == 1 {
            Ok(PaneExit::OpenNote)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = typed("session switch root");
    keys.push(Ok(Key::Enter)); // focus + attach root -> OpenNote
    keys.push(Ok(Key::Escape)); // (re-attached, now Focus) -> Overview
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *opened.borrow(),
        2,
        "the root pane is re-attached straight away"
    );
    assert!(recorded.borrow().is_empty());
}
