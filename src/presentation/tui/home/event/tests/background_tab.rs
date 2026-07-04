//! The background loading-tab flow: 在席's `terminal` / `agent` on a session that
//! already shows tabs spawns the pane through `spawn_pane_bg` (no attach), lands
//! in 切替 to watch it load, and the loop moves to it once ready — unless the user
//! acts first. These drive the loop with capturing background hooks (the pool-less
//! [`super::event_loop_compat`] can only no-op them).

use super::*;

/// Drive the loop with capturing background-pane hooks. Reaching 在席 on a live
/// session and pressing `t` dispatches a background terminal; the hooks then
/// decide how the loading tab resolves.
#[allow(clippy::too_many_arguments)]
fn run_bg(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path, Sidebar) -> Option<TerminalView>,
    spawn_pane_bg: &mut SpawnPaneBg,
    poll_pending: &mut PollPending,
    activate_pane: &mut dyn FnMut(&Path, u64) -> bool,
) -> Result<Outcome> {
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let tasks = TaskHandle::new();
    let update = UpdateHandle::new();
    let mut persist: fn(&crate::domain::history::HistoryEntry) = noop_persist_entry;
    let mut dispatch_create = |_: &Path, _: &str, _: u64| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut set_label_fake = |_: &Path, n: &str, id: Option<&str>| noop_set_label(n, id);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut dispatch_remove = |_: &Path, _: &str, _: bool, _| {};
    let mut evict = |_: &Path| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    // A live tab strip so 在席's `terminal` reads the session as already showing
    // tabs (the background path) and the 切替 preview has a strip to animate on.
    let mut tab_op = |_: &Path, _: Option<TabNav>| (vec!["terminal".to_string()], 0usize);
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut open_url: fn(&str) = noop_open_url;
    let mut dispatch_update = || {};
    let mut unite_resolve = no_unite_resolve;
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    let mut open_external_terminal = |_: &Path| Ok::<(), String>(());
    let mut chat_ask = ready_chat_ask;
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions: false,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        set_label: &mut set_label_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut dispatch_remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut dispatch_update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal,
        spawn_pane_bg,
        poll_pending,
        activate_pane,
        open_url: &mut open_url,
        open_external_terminal: &mut open_external_terminal,
        open_config: &mut config,
        chat_ask: &mut chat_ask,
        preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
    };
    event_loop(
        &term,
        &mut reader,
        state,
        &monitor,
        &update,
        &SessionsRefreshHandle::new(),
        &OneShot::<bool>::new(),
        &OneShot::<Vec<AgentCli>>::new(),
        &tasks,
        &mut wiring,
    )
}

/// Keys that reach 在席 on live `feat` and run its `terminal` command: switch to
/// feat, `Enter` to re-attach (zooming out to 在席 via `ToFocus`), then `t`.
fn reach_focus_and_launch_terminal() -> Vec<io::Result<Key>> {
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // re-attach live feat -> ToFocus -> 在席 menu
    keys.push(Ok(Key::Char('t'))); // run `terminal`: dispatched in the background
    keys
}

#[test]
fn a_ready_background_tab_is_attached_when_the_user_stays_idle() {
    // The menu's `terminal` spawns a background pane (id 7). Idle at the next poll
    // and already painted, so the loop activates that pane and re-attaches it —
    // moving to the new tab exactly as the requirement asks.
    let spawned = RefCell::new(0usize);
    let activated = RefCell::new(Vec::new());
    let opens = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, n: bool| {
        opens.borrow_mut().push((a, n));
        if opens.borrow().len() == 1 {
            Ok(PaneExit::ToFocus) // the initial re-attach zooms out to 在席
        } else {
            Ok(PaneExit::Closed) // the ready background tab, once attached
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut spawn = |_h: &mut HomeState, _d: &Path, _a: bool| {
        *spawned.borrow_mut() += 1;
        Ok(Some(7u64))
    };
    let mut poll = |_d: &Path, _id: u64| Some((0usize, true)); // ready on the first poll
    let mut activate = |_d: &Path, id: u64| {
        activated.borrow_mut().push(id);
        true
    };
    let outcome = run_bg(
        reach_focus_and_launch_terminal(),
        sample_state(),
        &mut open,
        &mut preview,
        &mut spawn,
        &mut poll,
        &mut activate,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *spawned.borrow(),
        1,
        "the terminal spawned one background pane"
    );
    assert_eq!(*activated.borrow(), vec![7], "the ready pane was activated");
    // Two attaches: the initial re-attach, then the now-active background tab.
    assert_eq!(*opens.borrow(), vec![(false, false), (false, false)]);
}

#[test]
fn acting_before_the_tab_is_ready_cancels_the_move() {
    // The background pane never reports ready; a keypress (↓) after the launch
    // bumps the interaction epoch, so the loop drops the pending tracker without
    // moving to the tab — it stays a background pane. Only the initial attach runs.
    let opens = RefCell::new(0usize);
    let activated = RefCell::new(0usize);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opens.borrow_mut() += 1;
        if *opens.borrow() == 1 {
            Ok(PaneExit::ToFocus)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut spawn = |_h: &mut HomeState, _d: &Path, _a: bool| Ok(Some(7u64));
    let mut poll = |_d: &Path, _id: u64| Some((0usize, false)); // never ready
    let mut activate = |_d: &Path, _id: u64| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut keys = reach_focus_and_launch_terminal();
    keys.push(Ok(Key::ArrowDown)); // acts while loading -> cancels the auto-move
    let outcome = run_bg(
        keys,
        sample_state(),
        &mut open,
        &mut preview,
        &mut spawn,
        &mut poll,
        &mut activate,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *activated.borrow(),
        0,
        "acting cancelled the move, so nothing was activated"
    );
    assert_eq!(
        *opens.borrow(),
        1,
        "only the initial attach ran; the tab stayed in the background"
    );
}

#[test]
fn a_vanished_background_tab_is_dropped() {
    // The background pane closes before it is ready (its shell died on spawn), so
    // the poll reports it gone: the loop drops the tracker without attaching.
    let activated = RefCell::new(0usize);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut spawn = |_h: &mut HomeState, _d: &Path, _a: bool| Ok(Some(7u64));
    let mut poll = |_d: &Path, _id: u64| None; // the pane vanished
    let mut activate = |_d: &Path, _id: u64| {
        *activated.borrow_mut() += 1;
        true
    };
    let outcome = run_bg(
        reach_focus_and_launch_terminal(),
        sample_state(),
        &mut open,
        &mut preview,
        &mut spawn,
        &mut poll,
        &mut activate,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*activated.borrow(), 0, "a vanished pane is never activated");
}

#[test]
fn a_failed_background_spawn_is_logged_and_stays_in_focus() {
    // When `spawn_pane_bg` errors, the launch is logged and no pending tab is
    // tracked — the loop never polls or activates anything.
    let activated = RefCell::new(0usize);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut spawn = |_h: &mut HomeState, _d: &Path, _a: bool| Err(anyhow::anyhow!("no shell"));
    let mut poll = |_d: &Path, _id: u64| Some((0usize, true));
    let mut activate = |_d: &Path, _id: u64| {
        *activated.borrow_mut() += 1;
        true
    };
    let outcome = run_bg(
        reach_focus_and_launch_terminal(),
        sample_state(),
        &mut open,
        &mut preview,
        &mut spawn,
        &mut poll,
        &mut activate,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *activated.borrow(),
        0,
        "a failed spawn tracks no pending tab"
    );
}
