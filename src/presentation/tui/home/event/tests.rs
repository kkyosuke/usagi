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

#[test]
fn a_restored_attached_engagement_auto_attaches_on_the_first_pass() {
    // `restore_focus` armed an Attached resume on a live session; the loop attaches
    // it once on entry — before reading any key — so the user lands back in 没入.
    let attached = RefCell::new(false);
    let mut open = |_: &mut HomeState, _: &Path, _: bool, _: bool| -> Result<PaneExit> {
        *attached.borrow_mut() = true;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut state = sample_state();
    state.restore_focus("feat", ResumeLevel::Attached);
    // No scripted keys: the loop auto-attaches on entry, then the default Ctrl-C
    // terminator quits (no live session under the detached monitor).
    let outcome = run_full(
        vec![],
        state,
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(
        *attached.borrow(),
        "the restored session should be attached on the first pass"
    );
}

#[test]
fn no_restored_engagement_leaves_the_first_pass_untouched() {
    // With nothing armed (the usual launch) the entry attach is a no-op: the loop
    // opens in 切替 and never drives a pane before the terminating Ctrl-C.
    let attached = RefCell::new(false);
    let mut open = |_: &mut HomeState, _: &Path, _: bool, _: bool| -> Result<PaneExit> {
        *attached.borrow_mut() = true;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let outcome = run_full(
        vec![],
        sample_state(),
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(
        !*attached.borrow(),
        "nothing armed: no pane should be driven"
    );
}

#[test]
fn a_key_press_is_traced_when_tracing_is_enabled() {
    // With tracing on, the loop builds and records the per-key trace event via
    // the `record_with` closure — the construction that is otherwise skipped (and
    // so left uncovered) while tracing is off in every other test. The data dir is
    // pinned to a temp home and the env mutation serialised, as the trace_log
    // tests do.
    let _guard = crate::test_support::process_env_guard();
    let home = tempfile::tempdir().unwrap();
    std::env::set_var(crate::infrastructure::storage::DATA_DIR_ENV, home.path());
    std::env::set_var(crate::infrastructure::trace_log::TRACE_ENV, "1");

    // One inert key (Esc at the base Switch) routes through the trace, then
    // Ctrl-C quits (no live session, so it exits at once).
    let outcome = run(vec![Ok(Key::Escape), Ok(Key::CtrlC)], sample_state()).unwrap();
    assert!(matches!(outcome, Outcome::Quit));

    // The press landed in today's trace file as a `tui` event.
    let traced = std::fs::read_dir(home.path().join("logs"))
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| {
            let path = e.path();
            path.extension().and_then(|x| x.to_str()) == Some("jsonl")
                && std::fs::read_to_string(&path)
                    .map(|c| c.contains("\"tui\""))
                    .unwrap_or(false)
        });
    assert!(traced, "the key press should be recorded to the trace log");

    std::env::remove_var(crate::infrastructure::trace_log::TRACE_ENV);
    std::env::remove_var(crate::infrastructure::storage::DATA_DIR_ENV);
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
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
    let mut keys = cmd("session switch root");
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
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

// --- base 切替 (Switch) -------------------------------------------------

#[test]
fn escape_at_the_base_switch_is_inert_and_does_not_leave() {
    // Esc no longer backs out to the project list: it is a no-op at the base
    // 切替 (the default), so the loop runs on and only the fallback Ctrl-C (no
    // live session) quits. A Back-returning Esc would instead resolve to
    // `Outcome::Back` here.
    assert!(matches!(
        run(vec![Ok(Key::Escape)], sample_state()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_c_at_the_base_switch_returns_quit() {
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
fn colon_opens_the_command_palette_from_the_base_switch() {
    // `:` at the base 切替 summons the command palette overlay; `Esc` closes it
    // back to 切替, where Esc is inert and the fallback Ctrl-C quits.
    let keys = vec![
        Ok(Key::Char(':')), // base Switch -> command palette
        Ok(Key::Escape),    // close the palette -> base Switch
        Ok(Key::Escape),    // Esc inert at the base Switch; fallback Ctrl-C quits
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn escape_in_switch_closes_the_note_before_backing_out() {
    // With the highlighted session's read-only note showing, the first Esc closes
    // the note and stays in 切替; a second Esc is then inert at the base 切替, and
    // the fallback Ctrl-C quits. The note's lifecycle is owned by Esc before the
    // mode's is. 切替 is the default, so no Ctrl-O is needed to reach it.
    let mut state = sample_state();
    state.restore_sessions(vec![SessionRecord {
        name: "alpha".to_string(),
        display_name: None,
        note: Some("todo".to_string()),
        root: PathBuf::from("/ws/.usagi/sessions/alpha"),
        worktrees: vec![worktree(Some("alpha"), "/ws/alpha")],
        created_at: Utc::now(),
    }]);
    let keys = vec![
        Ok(Key::ArrowDown), // root -> alpha; its note auto-shows
        Ok(Key::Escape),    // close the note (stays in Switch)
        Ok(Key::Escape),    // inert at the base Switch
        Ok(Key::Escape),    // still inert; fallback Ctrl-C quits
    ];
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn text_modal_scrolls_and_dismisses() {
    // `man` (run from the palette) opens a scrollable text modal over it; the
    // arrows / j/k and PageUp/PageDown scroll it, Esc dismisses it (back to the
    // palette), a `PageUp` then exercises the palette's no-op catch-all, and Esc
    // closes the palette (fallback Ctrl-C quits).
    let mut keys = cmd("man");
    keys.push(Ok(Key::Enter)); // run `man` -> opens the text modal over the palette
    keys.push(Ok(Key::ArrowDown)); // scroll down a line
    keys.push(Ok(Key::Char('j')));
    keys.push(Ok(Key::ArrowUp)); // scroll up a line
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::PageDown)); // page down
    keys.push(Ok(Key::PageUp)); // page up
    keys.push(Ok(Key::Char('z'))); // ignored inside the modal
    keys.push(Ok(Key::Escape)); // dismiss the modal -> back on the palette
    keys.push(Ok(Key::PageUp)); // a no-op key in the palette (its catch-all)
    keys.push(Ok(Key::Escape)); // close the palette; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn preview_command_opens_reads_scrolls_and_dismisses_the_markdown_pane() {
    // `preview <file>` resolves and reads the file under the workspace root, opens
    // the right-pane preview, and then the arrows / j/k and PageUp/PageDown scroll
    // it while Esc dismisses it (back to the base Switch, where Ctrl-C quits).
    let dir = tempfile::tempdir().unwrap();
    let body = (0..40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.path().join("README.md"), format!("# Title\n{body}")).unwrap();

    let mut keys = cmd("preview README");
    keys.push(Ok(Key::Enter)); // run `preview` -> reads the file, opens the pane
    keys.push(Ok(Key::ArrowDown)); // scroll down a line
    keys.push(Ok(Key::Char('j')));
    keys.push(Ok(Key::ArrowUp)); // scroll up a line
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::PageDown)); // page down
    keys.push(Ok(Key::PageUp)); // page up
    keys.push(Ok(Key::Char('z'))); // ignored inside the preview
    keys.push(Ok(Key::Escape)); // dismiss -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
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
    let mut keys = cmd("preview missing");
    keys.push(Ok(Key::Enter)); // run `preview` -> read fails, nothing opens
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch (no preview captured it)
    assert!(matches!(
        run_at(keys, sample_state(), dir.path()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn overview_edits_completes_and_recalls_then_runs() {
    let mut keys = cmd("ma");
    keys.push(Ok(Key::Backspace));
    keys.push(Ok(Key::Tab)); // "m" -> "man"
    keys.push(Ok(Key::Enter)); // run -> `man` opens its text modal
    keys.push(Ok(Key::Escape)); // dismiss the modal -> Switch
    keys.push(Ok(Key::ArrowUp)); // recall the previous command
    keys.push(Ok(Key::ArrowDown)); // back to empty
    keys.push(Ok(Key::Home)); // ignored
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn overview_caret_keys_edit_within_the_line() {
    // Build "history" by typing out of order and moving the caret with the
    // editing keys, exercising ←/→/End/Del; the recorded command proves the
    // edits landed where the caret was.
    let mut keys = cmd("hstory"); // missing the 'i'
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(recorded, vec!["history"]);
}

#[test]
fn quit_command_exits_the_app() {
    let mut keys = cmd("quit");
    keys.push(Ok(Key::Enter));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn submitted_commands_are_handed_to_persist() {
    let mut keys = cmd("man");
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(recorded, vec!["man"]);
}

#[test]
fn palette_refuses_session_scoped_commands() {
    // `terminal` / `agent` / `close` are session-scoped; the `:` palette is the
    // workspace surface, so dispatch refuses them (an error line, no action) and
    // the palette stays open. No pane is ever attached, however they are typed.
    let opened = RefCell::new(false);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() = true;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("terminal"); // open palette, type `terminal`
    keys.push(Ok(Key::Enter)); // refused -> palette stays open, input cleared
    keys.extend(typed("agent")); // type `agent` into the still-open palette
    keys.push(Ok(Key::Enter)); // refused
    keys.extend(typed("close")); // type `close`
    keys.push(Ok(Key::Enter)); // refused
    keys.push(Ok(Key::Escape)); // close the palette -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
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
    assert!(!*opened.borrow(), "no session command should attach a pane");
}

// --- Ctrl-^ jump to the previously focused session -----------------------

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

#[test]
fn ctrl_caret_in_overview_with_no_previous_session_is_a_no_op() {
    // Nothing has been focused yet, so the jump finds no target and the loop just
    // quits on the trailing Ctrl-C without ever attaching a pane.
    let dirs = run_capturing_attached_dirs(vec![Ok(Key::Char(CTRL_CARET)), Ok(Key::CtrlC)]);
    assert!(dirs.is_empty());
}

#[test]
fn ctrl_caret_on_the_base_switch_jumps_back_to_the_previous_session() {
    // Focus feat, then main; Ctrl-^ from the base 切替 re-attaches feat.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat -> Focus
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.extend(cmd("session switch main"));
    keys.push(Ok(Key::Enter)); // attach main (previous = feat) -> Focus
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Char(CTRL_CARET))); // jump back to feat
    let dirs = run_capturing_attached_dirs(keys);
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/r/feat"),
            PathBuf::from("/r/main"),
            PathBuf::from("/r/feat"),
        ]
    );
}

#[test]
fn ctrl_caret_in_switch_jumps_back_to_the_previous_session() {
    // Same setup, but the jump is issued from 切替 (reached via Ctrl-O from Focus).
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    keys.extend(cmd("session switch main"));
    keys.push(Ok(Key::Enter)); // Focus main (previous = feat)
    keys.push(Ok(Key::Char(CTRL_O))); // Focus -> Switch
    keys.push(Ok(Key::Char(CTRL_CARET))); // jump back to feat
    let dirs = run_capturing_attached_dirs(keys);
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/r/feat"),
            PathBuf::from("/r/main"),
            PathBuf::from("/r/feat"),
        ]
    );
}

#[test]
fn ctrl_caret_in_focus_jumps_back_to_the_previous_session() {
    // The jump is issued directly from 在席 (Focus) on the current session.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    keys.extend(cmd("session switch main"));
    keys.push(Ok(Key::Enter)); // Focus main (previous = feat)
    keys.push(Ok(Key::Char(CTRL_CARET))); // jump back to feat
    let dirs = run_capturing_attached_dirs(keys);
    assert_eq!(
        dirs,
        vec![
            PathBuf::from("/r/feat"),
            PathBuf::from("/r/main"),
            PathBuf::from("/r/feat"),
        ]
    );
}

#[test]
fn ctrl_caret_from_an_attached_pane_jumps_back_to_the_previous_session() {
    // From 没入, `Ctrl-^` surfaces as PaneExit::ToPreviousSession: attaching `main`
    // hands it back, and the loop re-roots on the previously focused `feat`.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, _a: bool, _n: bool| {
        opened.borrow_mut().push(d.to_path_buf());
        if d.ends_with("main") {
            Ok(PaneExit::ToPreviousSession)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat -> Focus
    keys.push(Ok(Key::Escape));
    keys.extend(cmd("session switch main"));
    keys.push(Ok(Key::Enter)); // attach main -> ToPreviousSession -> re-attach feat
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
    assert_eq!(
        *opened.borrow(),
        vec![
            PathBuf::from("/r/feat"),
            PathBuf::from("/r/main"),
            PathBuf::from("/r/feat"),
        ]
    );
}

#[test]
fn ctrl_caret_from_an_attached_pane_with_no_previous_falls_back_to_focus() {
    // Attaching the root (the first focus, recording no previous) and handing back
    // ToPreviousSession finds no target, so the pane drops to 在席 — exactly one
    // attach, no re-rooting.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, _a: bool, _n: bool| {
        opened.borrow_mut().push(d.to_path_buf());
        Ok(PaneExit::ToPreviousSession)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    // The root previews live (`live_preview`), so focusing it from Switch attaches
    // its pane directly — the first focus, recording no previous.
    let keys = vec![Ok(Key::Enter)]; // focus + attach root -> ToPreviousSession -> no target -> Focus
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
    assert_eq!(*opened.borrow(), vec![PathBuf::from("/ws")]);
}

#[test]
fn ctrl_q_from_an_attached_pane_raises_the_confirm_modal_instead_of_quitting() {
    // From 没入, `Ctrl-Q` surfaces as PaneExit::Quit: `open_pane` leaves the pane
    // and opens the quit-confirmation modal rather than quitting outright. The
    // first attach hands back Quit; cancelling the modal (`n`) and re-attaching
    // proves the app kept running — `open` is called a second time. A bug that
    // quit immediately (or merely detached, opening the note editor on `n`) would
    // never reach that second attach.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, _a: bool, _n: bool| {
        let count = {
            let mut v = opened.borrow_mut();
            v.push(d.to_path_buf());
            v.len()
        };
        if count == 1 {
            Ok(PaneExit::Quit)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = cmd("session switch main");
    keys.push(Ok(Key::Enter)); // attach main -> Quit -> leave + modal
    keys.push(Ok(Key::Char('n'))); // cancel the modal (keeps running)
    keys.extend(cmd("session switch main"));
    keys.push(Ok(Key::Enter)); // attach main again -> Closed -> Focus
    keys.push(Ok(Key::Char(CTRL_Q))); // raise the modal again from Focus
    keys.push(Ok(Key::Char('y'))); // confirm -> quit
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
    assert_eq!(
        *opened.borrow(),
        vec![PathBuf::from("/r/main"), PathBuf::from("/r/main")]
    );
}

#[test]
fn session_list_logs_the_sessions() {
    let mut keys = cmd("session list");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn session_list_with_sessions_opens_a_modal() {
    // With sessions recorded, `session list` opens the scrollable Sessions modal
    // (the empty-state path is a one-liner); Esc then dismisses it.
    let mut keys = cmd("session list");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    assert!(matches!(
        run(keys, state_with_sessions(&["alpha", "beta"])).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn session_create_with_a_name_creates_immediately() {
    let mut keys = cmd("session create newx");
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
    let mut keys = cmd("session create");
    keys.push(Ok(Key::Enter)); // -> Switch + begin create
    keys.extend(typed("wip"));
    keys.push(Ok(Key::Enter)); // confirm create -> Focus
    keys.push(Ok(Key::Escape)); // Focus Esc -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
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
fn a_finished_create_drops_into_focus_on_the_new_session() {
    // Creating a session from 統括 dispatches the git work to a worker; when it
    // finishes the loop drops straight into 在席 (Focus) on the new session, so the
    // user operates it without navigating over. We prove the landing by running the
    // 在席 menu's `terminal` (the `t` key, Focus-only) and observing it opens a pane
    // rooted at the new session's worktree — only possible if Focus is on `newx`.
    let mut keys = cmd("session create newx");
    keys.push(Ok(Key::Enter)); // dispatch create; completion drains next frame -> Focus(newx)
    keys.push(Ok(Key::Char('t'))); // 在席 menu: run `terminal` on the focused session
                                   // reader runs out -> Ctrl-C quits
    let opened = RefCell::new(Vec::new());
    let mut open = |_: &mut HomeState, dir: &Path, _: bool, _: bool| {
        opened.borrow_mut().push(dir.to_path_buf());
        Ok(PaneExit::Closed)
    };
    let mut create = |name: &str| SessionOutcome {
        line: LogLine::output("created"),
        // The refreshed list the worker reads back: the new session is present, so
        // the loop can match it by name and focus its row.
        sessions: Some(vec![
            SessionRecord {
                name: "main".to_string(),
                display_name: None,
                note: None,
                root: PathBuf::from("/ws/.usagi/sessions/main"),
                worktrees: vec![worktree(Some("main"), "/r/main")],
                created_at: Utc::now(),
            },
            SessionRecord {
                name: name.to_string(),
                display_name: None,
                note: None,
                root: PathBuf::from("/ws/.usagi/sessions/newx"),
                worktrees: vec![worktree(Some(name), "/r/newx")],
                created_at: Utc::now(),
            },
        ]),
        select: None,
    };
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
    // The pane roots at the new session's row (its session root), proving 在席
    // landed on `newx`.
    assert_eq!(
        opened.borrow().as_slice(),
        &[PathBuf::from("/ws/.usagi/sessions/newx")]
    );
}

#[test]
fn session_remove_with_a_name_and_force_routes_to_remove() {
    let mut keys = cmd("session remove old --force");
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(removed, vec![("old".to_string(), true)]);
}

#[test]
fn close_typed_on_the_root_in_focus_is_refused() {
    // `close` is session-scoped, so it reaches `close_focused_session` only from
    // the 在席 prompt (the palette refuses it — see `palette_refuses_session_scoped_commands`).
    // The focused row is the root by default, which is the workspace itself and
    // not a session, so `close` is refused outright: `remove` is never called and
    // the screen stays put.
    let mut keys = cmd("session switch root"); // focus the root row
    keys.push(Ok(Key::Enter)); // -> 在席 prompt (root)
    keys.extend(typed("close"));
    keys.push(Ok(Key::Enter)); // run `close` on the root -> refused
    keys.push(Ok(Key::Escape)); // 在席 -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
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
        prompt_state(), // 在席 prompt surface, so `close` can be typed on the root
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(
        removed.is_empty(),
        "close on the root row must not call remove"
    );
}

#[test]
fn focus_close_command_removes_the_focused_session_then_enters_switch() {
    // 在席 the `feat` session, then run `close` from the prompt: it removes the
    // focused session like `session remove feat` (no `--force`, so a dirty
    // worktree would be refused rather than discarded). Because the focused
    // session is now gone, the screen drops into 切替 (Switch) to pick the
    // next one. We prove the landing mode by pressing `c` — a Switch-only action
    // that opens the inline create input and consults the branch-name callback;
    // in 統括 the same key would just type a character and never call it.
    let mut keys = cmd("session switch feat");
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // Dispatched without force (`false`): a dirty session is refused, not discarded.
    assert_eq!(removed, vec![("feat".to_string(), false)]);
    assert_eq!(
        branches_called, 1,
        "`c` after close began inline create, so the screen is in 切替 (Switch)"
    );
}

#[test]
fn focus_menu_close_removes_the_focused_session_then_enters_switch() {
    // The 在席 menu lists `close` last; ArrowUp from the top wraps to it. Enter
    // removes the focused session like `session remove feat` (no `--force`), then
    // drops into 切替 (Switch) — the `c` keypress that follows opens the inline
    // create input (a Switch-only action), proving the landing mode.
    let mut keys = cmd("session switch feat");
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // Dispatched without force (`false`): a dirty session is refused, not discarded.
    assert_eq!(removed, vec![("feat".to_string(), false)]);
    assert_eq!(
        branches_called, 1,
        "`c` after close began inline create, so the screen is in 切替 (Switch)"
    );
}

// --- session-removal modal --------------------------------------------

#[test]
fn session_remove_without_a_name_opens_the_modal_and_bulk_removes() {
    let mut keys = cmd("session remove");
    keys.push(Ok(Key::Enter)); // open the modal
    keys.push(Ok(Key::Char(' '))); // check "alpha"
    keys.push(Ok(Key::ArrowDown));
    keys.push(Ok(Key::Char('j'))); // cursor on "gamma"
    keys.push(Ok(Key::Char(' '))); // check "gamma"
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::ArrowUp)); // cursor 0
    keys.push(Ok(Key::Home)); // ignored
    keys.push(Ok(Key::Enter)); // confirm
    keys.push(Ok(Key::Escape)); // back to the palette
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
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
    let mut keys = cmd("session remove");
    keys.push(Ok(Key::Enter)); // open
    keys.push(Ok(Key::Enter)); // nothing checked -> stays open
    keys.push(Ok(Key::Char(' '))); // check alpha
    keys.push(Ok(Key::Escape)); // cancel the modal
    keys.push(Ok(Key::Escape)); // back to the palette
    assert!(matches!(
        run(keys, state_with_sessions(&["alpha"])).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_c_in_the_removal_modal_quits() {
    let mut keys = cmd("session remove");
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
fn ctrl_q_at_the_base_switch_confirms_before_quitting() {
    // Unlike Ctrl-C, Ctrl-Q always raises the quit-confirmation modal first —
    // even with nothing live — so a lone Ctrl-Q does not quit; `y` then confirms.
    assert!(matches!(
        run(
            vec![Ok(Key::Char(CTRL_Q)), Ok(Key::Char('y'))],
            sample_state()
        )
        .unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_q_modal_can_be_cancelled_then_re_raised_and_confirmed() {
    // Ctrl-Q raises the modal on an idle screen; `n` cancels back to 切替 (proving
    // it did not quit, since the loop reads on); a second Ctrl-Q raises it again
    // and a third Ctrl-Q inside the modal confirms the close.
    let keys = vec![
        Ok(Key::Char(CTRL_Q)), // raise the modal (idle)
        Ok(Key::Char('n')),    // cancel -> 切替
        Ok(Key::Char(CTRL_Q)), // raise again
        Ok(Key::Char(CTRL_Q)), // confirm via a second Ctrl-Q inside the modal
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn ctrl_q_with_a_live_session_confirms_before_quitting() {
    // With a live session Ctrl-Q raises the same confirm modal; `y` confirms.
    let mut persist: fn(&str) = noop_persist;
    assert!(matches!(
        run_with_live_monitor(
            vec![Ok(Key::Char(CTRL_Q)), Ok(Key::Char('y'))],
            sample_state(),
            &mut persist,
        )
        .unwrap(),
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
    // in it; 'n' cancels back to 切替, where a palette command still runs (proving
    // the first Ctrl-C did not quit). Esc also cancels; Enter finally confirms.
    let mut keys = vec![
        Ok(Key::CtrlC),     // raise the modal
        Ok(Key::Home),      // ignored inside the modal
        Ok(Key::Char('n')), // cancel -> 切替
    ];
    keys.extend(cmd("man")); // open the palette and type `man`
    keys.push(Ok(Key::Enter)); // runs `man` -> persisted (opens a text modal)
    keys.push(Ok(Key::Escape)); // dismiss the text modal
    keys.push(Ok(Key::Escape)); // close the palette
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
    keys.extend(cmd("session create foo"));
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
    keys.extend(cmd("man"));
    keys.push(Ok(Key::Enter)); // runs `man` -> persisted (text modal over the palette)
    keys.push(Ok(Key::Escape)); // dismiss the text modal
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
    let mut keys = cmd("config");
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
    // new surface without reopening the screen. Focusing the root then running
    // `terminal` from the (now Prompt) 在席 surface attaches a pane, letting us
    // observe the live state's setting.
    let mut config = |_: &Term| Ok(Some(reload(SessionActionUi::Prompt)));
    let seen = RefCell::new(None);
    let mut open = |state: &mut HomeState, _: &Path, _: bool, _: bool| {
        *seen.borrow_mut() = Some(state.session_action_ui());
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("config");
    keys.push(Ok(Key::Enter)); // open config -> returns Prompt -> back to Switch
    keys.push(Ok(Key::Enter)); // focus root (idle) -> 在席 prompt (the new surface)
    keys.extend(typed("terminal")); // type into the 在席 prompt
    keys.push(Ok(Key::Enter)); // run terminal -> attach root, observing the setting
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
    let mut keys = cmd("config");
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

// --- session switch <name> (palette -> Focus / Attached) --------------

#[test]
fn session_switch_unknown_name_logs_an_error_and_keeps_the_palette_open() {
    // An unknown name does not resolve, so the palette stays open with the error
    // shown; `Esc` closes it, and the fallback Ctrl-C quits.
    let mut keys = cmd("session switch nope");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape)); // palette stays open; Esc closes it
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn session_switch_known_idle_name_enters_focus() {
    // "feat" resolves but is idle (no live preview), so it just enters Focus.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn session_switch_known_live_name_attaches_then_returns_to_focus() {
    // "root" resolves and is live, so it attaches; noop_open closes the pane,
    // returning to Focus, then Esc -> Switch (fallback Ctrl-C quits).
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // -> Focus -> attach -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
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
fn note_editor_opened_while_attached_refreshes_the_attached_terminal_surface() {
    // `Ctrl-E` in 没入 floats the note editor over the attached session's pane and
    // stays in Attached mode while it is open. The loop clears the terminal
    // surface every frame, so it must re-publish the attached session's snapshot
    // (and tab strip) for the modes that draw the embedded terminal — otherwise
    // the live terminal vanishes behind the box and the short fallback pane clips
    // the box's bottom border as the note grows. `tab_op` is only called from the
    // surface-refresh path (the liveness probe ignores its result), so a call for
    // the attached session's dir proves the surface was refreshed while editing.
    let mut preview =
        |_d: &Path, _s: Sidebar| Some(TerminalView::from_rows(vec!["$ echo hi".to_string()], None));
    let tab_dirs = RefCell::new(Vec::<PathBuf>::new());
    let mut tab_op = |d: &Path, _n: Option<TabNav>| {
        tab_dirs.borrow_mut().push(d.to_path_buf());
        (vec!["sh".to_string()], 0usize)
    };
    // First pane iteration leaves to open the note editor; the re-attach after
    // saving then closes, so the loop does not bounce back into the editor.
    let calls = RefCell::new(0u32);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        let mut c = calls.borrow_mut();
        *c += 1;
        if *c == 1 {
            Ok(PaneExit::OpenNote)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;

    let term = Term::stdout();
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // focus feat (live) -> attach -> OpenNote -> editor
    keys.push(Ok(Key::Char(CTRL_S))); // save & close -> re-attach -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch; fallback Ctrl-C quits
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(
        tab_dirs.borrow().iter().any(|d| d == Path::new("/r/feat")),
        "the attached session's surface must be re-published while the note editor floats over it",
    );
}

// --- 切替 (Switch) -----------------------------------------------------

#[test]
fn switch_navigates_and_backs_out_to_overview() {
    // `session switch` enters Switch; ↑/↓ (jk) move between sessions and ←/→ (hl)
    // between the highlighted session's tabs (a no-op with no panes here); Esc
    // returns to the base Switch (the origin); Esc is then inert, so the fallback Ctrl-C
    // quits.
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch (origin: the base Switch)
    keys.push(Ok(Key::ArrowDown));
    keys.push(Ok(Key::ArrowUp));
    keys.push(Ok(Key::Char('j')));
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::ArrowLeft)); // tab prev (no panes -> no-op)
    keys.push(Ok(Key::ArrowRight)); // tab next (no-op)
    keys.push(Ok(Key::Char('h'))); // tab prev via vim key (no-op)
    keys.push(Ok(Key::Char('l'))); // tab next via vim key (no-op)
    keys.push(Ok(Key::Home)); // ignored
    keys.push(Ok(Key::Escape)); // back to the base Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn switch_ctrl_o_is_inert_at_the_base_switch() {
    // 統括 (Overview) is gone, so `Ctrl-O` at the base 切替 has nowhere further out
    // to zoom: it is a no-op and the screen stays in Switch (exhausting the script
    // falls back to Ctrl-C, which quits with nothing live).
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> base Switch
    keys.push(Ok(Key::Char(CTRL_O))); // no-op at the base Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn switch_snapshots_the_highlighted_live_session_for_the_preview() {
    // In 切替 the render loop snapshots the highlighted session's live
    // terminal so the right pane previews the actual screen. Under the live
    // harness `preview` returns a snapshot, exercising that surface-drive path.
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::ArrowDown)); // move onto a live session row
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
    assert!(matches!(
        run_live(keys, sample_state()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn switch_ctrl_c_quits() {
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn switch_enter_on_an_idle_session_just_focuses_it() {
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::ArrowDown)); // cursor on "main"
    keys.push(Ok(Key::Enter)); // focus (idle -> no attach)
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::Enter)); // focus + attach (live)
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::Char('t'))); // -> 在席 action surface (Menu)
    keys.push(Ok(Key::Char('t'))); // menu: run terminal -> adds a new pane
    keys.push(Ok(Key::Escape)); // 在席 -> Switch
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
    let mut keys = cmd("session switch");
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
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
    let mut keys = cmd("session switch");
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*closed.borrow(), vec![PathBuf::from("/r/main")]);
}

#[test]
fn switch_inline_create_makes_and_focuses_the_new_session() {
    let mut keys = cmd("session switch");
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
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
    // Cancel path: Esc closes the input, staying in Switch; then Ctrl-O -> Switch (fallback Ctrl-C quits).
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Char('c'))); // begin create
    keys.push(Ok(Key::Char('x')));
    keys.push(Ok(Key::Escape)); // cancel create (stay in Switch)
    keys.push(Ok(Key::Char(CTRL_O))); // inert at the base Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));

    // Ctrl-C inside the create input quits.
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Char('c')));
    keys.push(Ok(Key::CtrlC));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn switch_create_invalid_name_keeps_the_input_open() {
    // An empty confirm keeps the input open; then Ctrl-C ends the run.
    let mut keys = cmd("session switch");
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    (renamed.into_inner(), outcome)
}

#[test]
fn switch_inline_rename_edits_then_confirms_the_label() {
    // Switch -> cursor onto "main" -> `r` (prefills "main") -> mid-string edit
    // exercising the same caret keys as create (Home/End/←/→/Del/Backspace) ->
    // type "Top" -> Enter persists via the rename callback.
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch
    keys.push(Ok(Key::ArrowDown)); // cursor "main"
    keys.push(Ok(Key::Char('r'))); // begin rename (prefilled "main", caret at end)
    keys.push(Ok(Key::ArrowUp)); // a non-edit key is ignored while renaming
    keys.push(Ok(Key::Home)); // caret to the start
    keys.push(Ok(Key::Del)); // forward-delete 'm' -> "ain"
    keys.push(Ok(Key::End)); // caret to the end
    keys.push(Ok(Key::ArrowLeft)); // caret before 'n'
    keys.push(Ok(Key::ArrowRight)); // caret after 'n' (end)
    for _ in 0..3 {
        keys.push(Ok(Key::Backspace)); // clear "ain"
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
    let mut keys = cmd("session switch");
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
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch (cursor on root)
    keys.push(Ok(Key::Char('r'))); // no-op on root
    keys.push(Ok(Key::CtrlC));
    let (renamed, outcome) = run_recording_rename(keys);
    assert!(matches!(outcome, Outcome::Quit));
    assert!(renamed.is_empty());
}

// --- 切替 (Switch) reorder (K / J) -------------------------------------

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

#[test]
fn switch_reorder_moves_the_selected_session_up_and_down() {
    // J moves the selected session down, K moves it up. With a Stationary
    // response the cursor is undisturbed, so the scripted navigation reaches the
    // next session as written.
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch (cursor on root)
    keys.push(Ok(Key::ArrowDown)); // cursor "main"
    keys.push(Ok(Key::Char('J'))); // move "main" down
    keys.push(Ok(Key::ArrowDown)); // cursor "feat"
    keys.push(Ok(Key::Char('K'))); // move "feat" up
    keys.push(Ok(Key::CtrlC));
    let (moves, outcome) = run_recording_reorder(keys, SessionReorder::Stationary);
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        moves,
        vec![("main".to_string(), false), ("feat".to_string(), true)]
    );
}

#[test]
fn switch_reorder_on_the_root_row_is_a_noop() {
    // K / J on the root row (not a session) never reach the reorder callback.
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // -> Switch (cursor on root)
    keys.push(Ok(Key::Char('K')));
    keys.push(Ok(Key::Char('J')));
    keys.push(Ok(Key::CtrlC));
    let (moves, outcome) = run_recording_reorder(keys, SessionReorder::Stationary);
    assert!(matches!(outcome, Outcome::Quit));
    assert!(moves.is_empty());
}

#[test]
fn switch_reorder_applies_a_moved_result_and_logs_a_failure() {
    // A Moved result refreshes the pane (the reordered list is applied).
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::ArrowDown)); // cursor "main"
    keys.push(Ok(Key::Char('K')));
    keys.push(Ok(Key::CtrlC));
    let reordered = vec![
        SessionRecord {
            name: "feat".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/ws/.usagi/sessions/feat"),
            worktrees: vec![worktree(Some("feat"), "/ws/feat")],
            created_at: Utc::now(),
        },
        SessionRecord {
            name: "main".to_string(),
            display_name: None,
            note: None,
            root: PathBuf::from("/ws/.usagi/sessions/main"),
            worktrees: vec![worktree(Some("main"), "/ws/main")],
            created_at: Utc::now(),
        },
    ];
    let (moves, outcome) = run_recording_reorder(keys, SessionReorder::Moved(reordered));
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(moves, vec![("main".to_string(), true)]);

    // A Failed result is logged rather than panicking, and the run continues.
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::ArrowDown)); // cursor "main"
    keys.push(Ok(Key::Char('J')));
    keys.push(Ok(Key::CtrlC));
    let (moves, outcome) =
        run_recording_reorder(keys, SessionReorder::Failed(LogLine::error("boom")));
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(moves, vec![("main".to_string(), false)]);
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
    let mut keys = cmd("session switch");
    keys.push(Ok(Key::Enter)); // Switch
    keys.push(Ok(Key::ArrowDown)); // cursor "main" (/r/main)
    keys.push(Ok(Key::Enter)); // focus main (idle)
    keys.push(Ok(Key::Char('j'))); // terminal -> agent
    keys.push(Ok(Key::ArrowUp)); // agent -> terminal
    keys.push(Ok(Key::Enter)); // run terminal (attach) -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char('t'))); // terminal
    keys.push(Ok(Key::Char('k'))); // a menu move (no-op effect here)
    keys.push(Ok(Key::Char('a'))); // agent
    keys.push(Ok(Key::Escape)); // -> Switch
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
fn focus_menu_agent_picker_launches_the_chosen_cli() {
    use crate::domain::settings::AgentCli;
    // The fake pane reads the recorded choice the way the real wiring does.
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((a, h.take_agent_choice()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = sample_state();
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::ArrowDown)); // terminal -> agent
    keys.push(Ok(Key::ArrowRight)); // expand picker (default Claude highlighted)
    keys.push(Ok(Key::ArrowDown)); // Claude -> Codex
    keys.push(Ok(Key::Enter)); // launch Codex
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![(true, Some(AgentCli::Codex))]);
}

#[test]
fn focus_menu_agent_picker_collapses_on_left_and_esc_without_launching() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push(a);
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = sample_state();
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::Codex]);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::ArrowDown)); // terminal -> agent
    keys.push(Ok(Key::ArrowRight)); // expand
    keys.push(Ok(Key::ArrowUp)); // move within the picker (wraps)
    keys.push(Ok(Key::Char('k'))); // move within the picker (vim up)
    keys.push(Ok(Key::Home)); // an unhandled picker key: inert
    keys.push(Ok(Key::ArrowLeft)); // collapse (no launch)
    keys.push(Ok(Key::ArrowRight)); // expand again
    keys.push(Ok(Key::Escape)); // Esc collapses (no launch, stays in Focus)
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    // The picker only ever expanded/collapsed; no pane was launched.
    assert!(opened.borrow().is_empty());
}

#[test]
fn typed_agent_name_launches_an_installed_cli_but_refuses_an_uninstalled_one() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((a, h.take_agent_choice()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = prompt_state(); // 在席 prompt is where `agent <name>` is typed
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(vec![AgentCli::Codex]);
    // Focus an idle session to reach its 在席 prompt, then type `agent` there.
    // `agent gemini` (not installed, not the default) is refused — no launch — and
    // the prompt stays open, so `agent codex` (installed) can be typed next and
    // launches Codex.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat (prompt)
    keys.extend(typed("agent gemini"));
    keys.push(Ok(Key::Enter)); // refused -> stays in the 在席 prompt
    keys.extend(typed("agent codex"));
    keys.push(Ok(Key::Enter)); // launches Codex -> Closed -> 在席 prompt
    keys.push(Ok(Key::Escape)); // -> 切替
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![(true, Some(AgentCli::Codex))]);
}

#[test]
fn typed_agent_name_allows_the_default_cli_even_when_not_probed_as_installed() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((a, h.take_agent_choice()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = prompt_state(); // 在席 prompt is where `agent <name>` is typed
    state.set_default_agent(AgentCli::Claude);
    state.set_installed_agents(Vec::new()); // nothing probed as installed
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat (prompt)
    keys.extend(typed("agent claude")); // the configured default by name
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // quit
    assert!(matches!(
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![(true, Some(AgentCli::Claude))]);
}

#[test]
fn focus_menu_can_run_the_coming_soon_ai_command() {
    // With the local LLM available the menu lists terminal (0, default),
    // agent (1), ai (2), close (3). ArrowUp from the top wraps to "close"; one
    // more lands on "ai"; Enter on it just logs (no attach).
    let mut state = sample_state();
    state.set_ai_available(true);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus
    keys.push(Ok(Key::Home)); // ignored in the menu
    keys.push(Ok(Key::ArrowDown)); // terminal -> agent
    keys.push(Ok(Key::ArrowUp)); // back to terminal
    keys.push(Ok(Key::ArrowUp)); // wrap to "close"
    keys.push(Ok(Key::ArrowUp)); // up to "ai"
    keys.push(Ok(Key::Enter)); // run ai (coming soon)
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn focus_ctrl_o_opens_switch_then_esc_re_focuses() {
    // Focus -> Ctrl-O -> Switch(return=Focus); Esc re-enters Focus; Esc ->
    // base Switch; Esc inert, fallback Ctrl-C quits.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(CTRL_O))); // -> Switch(return Focus)
    keys.push(Ok(Key::Escape)); // back -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> base Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn focus_colon_opens_the_command_palette_then_esc_returns_to_focus() {
    // `:` in 在席 summons the command palette over the focus surface; `Esc` closes
    // it back to 在席, where `Esc` again leaves for the base 切替.
    let opened = RefCell::new(0);
    let mut config = |_: &Term| {
        *opened.borrow_mut() += 1;
        Ok(Some(reload(SessionActionUi::Menu)))
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus feat
    keys.push(Ok(Key::Char(':'))); // -> command palette over Focus
    keys.extend(typed("config")); // type into the palette
    keys.push(Ok(Key::Enter)); // run config (palette closes) -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> base Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            sample_state(),
            &mut open,
            &mut create,
            &mut preview,
            &mut config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), 1, "the palette ran the config command");
}

#[test]
fn focus_ctrl_n_and_ctrl_p_walk_the_tab_strip_via_tab_op() {
    // In 在席 with live panes, Ctrl-N / Ctrl-P walk the focused session's pane
    // tabs by making the chosen pane active through `tab_op` (`To(index)`), so its
    // preview shows and a re-attach lands on it — and they stay in Focus. The
    // session is reached live (a pane open, then `Ctrl-T` zooms out to Focus
    // keeping the panes alive), so the tab strip is published.
    let term = Term::stdout();
    let navs = RefCell::new(Vec::new());
    // A stateful tab strip of two panes that applies each `To(index)` so the next
    // frame's read reflects the move (the real pool behaves this way).
    let active = RefCell::new(0usize);
    let mut tab_op = |_d: &Path, nav: Option<TabNav>| -> (Vec<String>, usize) {
        if let Some(n) = nav {
            navs.borrow_mut().push(n);
            if let TabNav::To(i) = n {
                *active.borrow_mut() = i;
            }
        }
        (
            vec!["agent".to_string(), "terminal".to_string()],
            *active.borrow(),
        )
    };
    // Entering Focus on a live session attaches; `Ctrl-T` (ToFocus) zooms back out
    // to Focus with the panes still alive, which is where the tab strip shows.
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat; open returns ToFocus -> Focus on "+ new"
    keys.push(Ok(Key::Char(CTRL_N))); // "+ new" wraps to pane 0: To(0)
    keys.push(Ok(Key::Char(CTRL_N))); // pane 0 -> pane 1: To(1)
    keys.push(Ok(Key::Char(CTRL_P))); // pane 1 -> pane 0: To(0)
    keys.push(Ok(Key::CtrlC)); // quit
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    // A live preview so the surface drive publishes the tab strip while in Focus.
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *navs.borrow(),
        vec![TabNav::To(0), TabNav::To(1), TabNav::To(0)]
    );
}

#[test]
fn focus_tab_nav_is_inert_without_live_panes() {
    // An idle focused session (no live panes, so only the "+ new" tab) has nothing
    // to walk: Ctrl-N / Ctrl-P make no `tab_op` call and stay on the action surface.
    let term = Term::stdout();
    let navs = RefCell::new(Vec::new());
    let mut tab_op = |_d: &Path, nav: Option<TabNav>| -> (Vec<String>, usize) {
        if let Some(n) = nav {
            navs.borrow_mut().push(n);
        }
        (Vec::new(), 0)
    };
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus feat (idle: noop_preview is not live)
    keys.push(Ok(Key::Char(CTRL_N)));
    keys.push(Ok(Key::Char(CTRL_P)));
    keys.push(Ok(Key::CtrlC));
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(navs.borrow().is_empty());
}

#[test]
fn focus_enter_on_a_pane_tab_reattaches_while_other_keys_are_inert() {
    // In 在席 on a pane tab (reached by `Ctrl-T` from 没入, which lands on "+ new",
    // then `Ctrl-N` onto a pane tab), `Enter` re-attaches the selected pane
    // (`open_terminal` with `new_pane = false`); a non-`Enter` key there is inert
    // (the action surface only drives the "+ new" tab).
    let term = Term::stdout();
    let opens = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, agent: bool, new_pane: bool| {
        let count = {
            let mut o = opens.borrow_mut();
            o.push((agent, new_pane));
            o.len()
        };
        // The first attach (from focusing the live session) zooms out to Focus with
        // the panes kept alive; the re-attach then drops straight back out.
        if count == 1 {
            Ok(PaneExit::ToFocus)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut tab_op = |_d: &Path, _nav: Option<TabNav>| -> (Vec<String>, usize) {
        (vec!["agent".to_string(), "terminal".to_string()], 0)
    };
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat; open #1 -> ToFocus -> Focus on "+ new"
    keys.push(Ok(Key::Char(CTRL_N))); // "+ new" -> pane 0: now a pane tab is selected
    keys.push(Ok(Key::Char('j'))); // on a pane tab: inert, no open
    keys.push(Ok(Key::Enter)); // re-attach the selected pane; open #2 (false, false)
    keys.push(Ok(Key::CtrlC));
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // Two attaches: the initial focus-and-attach, then the `Enter` re-attach — the
    // `j` between them opened nothing. Both go in with `new_pane = false`.
    assert_eq!(*opens.borrow(), vec![(false, false), (false, false)]);
}

#[test]
fn focus_esc_on_the_new_tab_over_panes_steps_back_onto_the_pane() {
    // In 在席 on the "+ new" tab opened over live panes (`Ctrl-T` from 没入), `Esc`
    // discards the launch surface and steps back onto the active pane's tab —
    // staying in Focus, not zooming out to 統括. A following `Enter` re-attaches
    // that pane, proving the selector landed on a pane tab (not "+ new", whose
    // `Enter` would open a fresh pane with `new_pane = true`).
    let term = Term::stdout();
    let opens = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, agent: bool, new_pane: bool| {
        let count = {
            let mut o = opens.borrow_mut();
            o.push((agent, new_pane));
            o.len()
        };
        if count == 1 {
            Ok(PaneExit::ToFocus)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut tab_op = |_d: &Path, _nav: Option<TabNav>| -> (Vec<String>, usize) {
        (vec!["agent".to_string(), "terminal".to_string()], 0)
    };
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat; open #1 -> ToFocus -> Focus on "+ new"
    keys.push(Ok(Key::Escape)); // discard "+ new" -> step onto the active pane tab
    keys.push(Ok(Key::Enter)); // re-attach the pane; open #2 (false, false)
    keys.push(Ok(Key::CtrlC));
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // The `Esc` opened nothing (it stayed in Focus); the trailing `Enter`
    // re-attached the pane it stepped onto, both with `new_pane = false`.
    assert_eq!(*opens.borrow(), vec![(false, false), (false, false)]);
}

#[test]
fn focus_ctrl_c_quits() {
    let mut keys = cmd("session switch feat");
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
    let mut keys = cmd("session switch feat");
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
    keys.push(Ok(Key::Escape)); // -> Switch
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
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus (prompt)
    keys.push(Ok(Key::Home)); // ignored in the prompt
    keys.push(Ok(Key::Enter)); // empty prompt -> no-op
    keys.extend(typed("ai go"));
    keys.push(Ok(Key::Enter)); // coming soon -> log, no attach
    keys.extend(typed("agent"));
    keys.push(Ok(Key::Enter)); // attach agent
    keys.push(Ok(Key::Escape)); // -> Switch
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

#[test]
fn focus_prompt_agent_with_a_name_launches_that_cli() {
    use crate::domain::settings::AgentCli;
    let opened = RefCell::new(Vec::new());
    let mut open = |h: &mut HomeState, _d: &Path, a: bool, _n: bool| {
        opened.borrow_mut().push((a, h.take_agent_choice()));
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut state = prompt_state();
    state.set_installed_agents(vec![AgentCli::Claude, AgentCli::CodexFugu]);
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // Focus (prompt)
    keys.extend(typed("agent sakana.ai")); // pick the codex-fugu CLI by display name
    keys.push(Ok(Key::Enter)); // attach that agent
    keys.push(Ok(Key::Escape)); // -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full(
            keys,
            state,
            &mut open,
            &mut create,
            &mut preview,
            &mut noop_config
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*opened.borrow(), vec![(true, Some(AgentCli::CodexFugu))]);
}

// --- 没入 (Attached) exits ---------------------------------------------

#[test]
fn ctrl_o_in_the_pane_zooms_out_to_switch() {
    // Attaching to a live session; the pane returns ToSwitch (Ctrl-O), so the
    // loop enters Switch with return=Attached. Then Ctrl-O -> Switch (fallback Ctrl-C quits).
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToSwitch);
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // Focus root -> attach -> ToSwitch -> Switch
    keys.push(Ok(Key::Char(CTRL_O))); // inert at the base Switch
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
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // attach -> ToSwitch -> Switch(return Attached)
    keys.push(Ok(Key::Escape)); // Switch Esc -> re-attach -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // attach root -> ToSwitch -> Switch(return Attached)
    keys.push(Ok(Key::ArrowDown)); // cursor -> an idle worktree row
    keys.push(Ok(Key::Escape)); // Esc -> idle row stays in Focus (no re-attach)
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
fn ctrl_t_in_the_pane_zooms_out_to_focus() {
    // Attaching to a live session; the pane returns ToFocus (Ctrl-T), so the loop
    // leaves 没入 for 在席 (Focus) — the session's action menu — leaving the pane
    // alive. From Focus, Esc -> Switch (then Esc is inert; fallback Ctrl-C quits).
    // The pane opens exactly once: ToFocus does not spawn or re-attach a pane.
    let calls = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *calls.borrow_mut() += 1;
        Ok(PaneExit::ToFocus)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // Focus root -> attach -> ToFocus -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
    assert_eq!(*calls.borrow(), 1);
}

#[test]
fn pane_failure_is_reported_and_returns_to_focus() {
    let mut open =
        |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Err(anyhow::anyhow!("no shell"));
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // attach -> Err -> Focus (logged)
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
            focus: None,
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
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // live -> attach via noop_open -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
    assert!(matches!(
        run_live(keys, sample_state()).unwrap(),
        Outcome::Quit
    ));

    // `config` through the default `noop_config` (returns false -> resume).
    let mut keys = cmd("config");
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
        &mut (noop_reorder as fn(&str, bool) -> SessionReorder),
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
        Ok(Key::Char(CTRL_O)),     // no-op at base Switch (cursor already on root)
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
    keys.push(Ok(Key::Escape)); // inert at the base Switch
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

/// The fragments a terminal sends for `Shift`+`<letter>` cursor key, reassembled
/// by `term_reader` into the single `UnknownEscSeq` the loop sees: `CSI 1 ; 2
/// <letter>`. `letter` is the CSI final byte (`C` right, `D` left, `A` up, `B`
/// down, `H` home, `F` end).
fn shift_arrow(letter: char) -> io::Result<Key> {
    Ok(Key::UnknownEscSeq(vec!['[', '1', ';', '2', letter]))
}

#[test]
fn shift_arrows_select_text_and_delete_removes_the_selection() {
    // In the note editor, `Shift`+a cursor key extends a selection and `Del`
    // removes the whole span. Every selection direction is exercised, then the
    // surviving text is saved through `set_note`.
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
        Ok(Key::Char(CTRL_O)), // no-op at base Switch (cursor already on root)
        Ok(Key::ArrowDown),    // root -> alpha
        Ok(Key::Char('n')),    // open the note editor for alpha
    ];
    keys.extend(typed("hello world"));
    keys.push(Ok(Key::Home)); // caret to the line start (clears any selection)
    keys.push(shift_arrow('B')); // Shift+Down: single line, an empty extend
    keys.push(shift_arrow('A')); // Shift+Up: likewise
    keys.push(shift_arrow('F')); // Shift+End: select the whole line
    keys.push(shift_arrow('H')); // Shift+Home: collapse back to the start
    keys.push(shift_arrow('C')); // Shift+Right x5: select "hello"
    keys.push(shift_arrow('C'));
    keys.push(shift_arrow('C'));
    keys.push(shift_arrow('C'));
    keys.push(shift_arrow('C'));
    keys.push(shift_arrow('D')); // Shift+Left: shrink to "hell"
    keys.push(Ok(Key::Del)); // delete the selection -> "o world"
    keys.push(Ok(Key::Char(CTRL_S))); // save
    keys.push(Ok(Key::Escape)); // inert at the base Switch
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
        vec![("alpha".to_string(), "o world".to_string())]
    );
}

#[test]
fn switch_ctrl_e_opens_the_note_editor_like_n() {
    // 切替, `Ctrl-E` (matching 在席 / 没入) opens the highlighted session's note
    // editor just like `n`; Ctrl-S persists it through `set_note`.
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
        Ok(Key::Char(CTRL_O)), // no-op at base Switch (cursor already on root)
        Ok(Key::ArrowDown),    // root -> alpha
        Ok(Key::Char(CTRL_E)), // open the note editor for alpha
    ];
    keys.extend(typed("hi"));
    keys.push(Ok(Key::Char(CTRL_S))); // save
    keys.push(Ok(Key::Escape)); // inert at the base Switch
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
        vec![("alpha".to_string(), "hi".to_string())]
    );
}

#[test]
fn switch_end_key_opens_the_note_editor_like_ctrl_e() {
    // `console` decodes Ctrl-E as `Key::End`, so on a real terminal the chord
    // arrives as `End`; in 切替 list navigation (no caret) it opens the note just
    // like `Ctrl-E` / `n`.
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
        Ok(Key::Char(CTRL_O)), // no-op at base Switch (cursor already on root)
        Ok(Key::ArrowDown),    // root -> alpha
        Ok(Key::End),          // Ctrl-E as console delivers it: open the note
    ];
    keys.extend(typed("hi"));
    keys.push(Ok(Key::Char(CTRL_S))); // save
    keys.push(Ok(Key::Escape)); // inert at the base Switch
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
        vec![("alpha".to_string(), "hi".to_string())]
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
        Ok(Key::Char(CTRL_O)), // no-op at base Switch
        Ok(Key::ArrowDown),    // root -> alpha
        Ok(Key::Char('n')),    // open the editor
    ];
    keys.extend(typed("draft"));
    keys.push(Ok(Key::Escape)); // cancel the editor (no save)
    keys.push(Ok(Key::Escape)); // inert at the base Switch
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

    let mut keys = cmd("session switch alpha");
    keys.push(Ok(Key::Enter)); // focus + attach alpha -> open_terminal #1 -> OpenNote
    keys.extend(typed("hi")); // edit the note in the editor
    keys.push(Ok(Key::Char(CTRL_S))); // save -> re-attach -> open_terminal #2 -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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

    let mut keys = cmd("session switch alpha");
    keys.push(Ok(Key::Enter)); // attach -> OpenNote
    keys.extend(typed("scratch"));
    keys.push(Ok(Key::Escape)); // cancel -> re-attach -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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

    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // focus + attach root -> OpenNote
    keys.push(Ok(Key::Escape)); // (re-attached, now Focus) -> Switch
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

#[test]
fn focus_ctrl_e_opens_the_note_editor_and_saves_staying_in_focus() {
    // In 在席 (Focus), Ctrl-E opens the focused session's note editor; saving
    // persists the note and returns to 在席 (no pane to re-attach). We prove the
    // landing mode by pressing `t` afterwards — a 在席 menu shortcut that launches
    // a terminal — so the pane callback runs only if we are still in Focus.
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = cmd("session switch alpha");
    keys.push(Ok(Key::Enter)); // focus alpha (idle -> 在席 menu)
    keys.push(Ok(Key::Char(CTRL_E))); // open the note editor (reattach = false)
    keys.extend(typed("todo"));
    keys.push(Ok(Key::Char(CTRL_S))); // save -> back to 在席
    keys.push(Ok(Key::Char('t'))); // 在席 menu: launch terminal (proves we are in Focus)
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
        *recorded.borrow(),
        vec![("alpha".to_string(), "todo".to_string())]
    );
    assert_eq!(
        *opened.borrow(),
        1,
        "`t` after save launched a terminal, so we stayed in 在席"
    );
}

#[test]
fn focus_end_key_opens_the_note_editor_on_the_menu_surface() {
    // `console` decodes Ctrl-E as `Key::End`, so on a real terminal the chord
    // arrives as `End`. On 在席's menu surface (the default — no caret) it opens
    // the note just like the scripted `Ctrl-E`. (The typed prompt keeps `End` as
    // end-of-line; that path is covered by `focus_prompt_edits_*`.)
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = cmd("session switch alpha");
    keys.push(Ok(Key::Enter)); // focus alpha (idle -> 在席 menu)
    keys.push(Ok(Key::End)); // Ctrl-E as console delivers it: open the note
    keys.extend(typed("todo"));
    keys.push(Ok(Key::Char(CTRL_S))); // save -> back to 在席
    keys.push(Ok(Key::Char('t'))); // 在席 menu: launch terminal (proves we are in Focus)
    keys.push(Ok(Key::Escape)); // Focus -> Switch
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
        *recorded.borrow(),
        vec![("alpha".to_string(), "todo".to_string())]
    );
    assert_eq!(
        *opened.borrow(),
        1,
        "`t` after save launched a terminal, so we stayed in 在席"
    );
}
