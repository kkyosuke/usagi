//! The background loading-tab flow: 在席's `terminal` / `agent` on a session that
//! already shows tabs dispatches the pane through `start_pending_spawn` (no attach,
//! no centre loader), stays in 在席 with a loading tab in the strip, and the loop
//! moves to it once ready — unless the user acts first. These drive the loop with
//! capturing background hooks (the pool-less [`super::event_loop_compat`] only
//! no-ops them). Each test pins one loop branch: the fake `poll_pending_spawn`
//! returns a fixed phase, since [`ScriptedReader`] cancels on the next key.

use super::*;
use std::cell::RefCell;

/// Drive the loop with capturing background-pane hooks. Reaching 在席 on a live
/// session and pressing `t` dispatches a background terminal; the hooks then
/// decide how the loading tab resolves.
#[allow(clippy::too_many_arguments)]
fn run_bg(
    keys: Vec<io::Result<Key>>,
    state: HomeState,
    open_terminal: &mut dyn FnMut(&mut HomeState, &Path, bool, bool) -> Result<PaneExit>,
    preview: &mut dyn FnMut(&Path, Sidebar) -> Option<TerminalView>,
    start_pending_spawn: &mut StartPendingFn,
    poll_pending_spawn: &mut dyn FnMut(&Path) -> PendingPoll,
    activate_pending: &mut dyn FnMut(&Path) -> bool,
    clear_pending_spawn: &mut dyn FnMut(),
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
    // tabs (the background path) and its preview has a strip to animate on.
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
        start_pending_spawn,
        poll_pending_spawn,
        activate_pending,
        clear_pending_spawn,
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

/// A fake `start_pending_spawn` that reports a new background launch is in flight.
fn pending_start(_: &mut HomeState, _: &Path, _: bool) -> anyhow::Result<StartPending> {
    Ok(StartPending::Pending {
        label: "terminal".to_string(),
    })
}

#[test]
fn a_ready_background_tab_is_attached_when_the_user_stays_idle() {
    // The menu's `terminal` dispatches a background launch; idle at the next poll
    // and already ready, so the loop activates it and re-attaches — moving to the
    // new tab exactly as the requirement asks.
    let activated = RefCell::new(0usize);
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
    let mut start = pending_start;
    let mut poll = |_d: &Path| PendingPoll::Ready;
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {};
    let outcome = run_bg(
        reach_focus_and_launch_terminal(),
        sample_state(),
        &mut open,
        &mut preview,
        &mut start,
        &mut poll,
        &mut activate,
        &mut clear,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*activated.borrow(), 1, "the ready pane was activated");
    // Two attaches: the initial re-attach, then the now-active background tab.
    assert_eq!(*opens.borrow(), vec![(false, false), (false, false)]);
}

#[test]
fn a_starting_background_tab_animates_without_moving() {
    // While the pane is starting (spawned, shell not yet painted) the loop animates
    // its chip but does not move to it.
    let activated = RefCell::new(0usize);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut start = pending_start;
    let mut poll = |_d: &Path| PendingPoll::Starting(1);
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {};
    let outcome = run_bg(
        reach_focus_and_launch_terminal(),
        sample_state(),
        &mut open,
        &mut preview,
        &mut start,
        &mut poll,
        &mut activate,
        &mut clear,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *activated.borrow(),
        0,
        "a starting pane is not moved to yet"
    );
}

#[test]
fn a_resolving_background_tab_shows_a_placeholder_chip() {
    // While the environment resolves there is no pool pane yet, so the loop shows a
    // synthetic placeholder chip; nothing is activated.
    let activated = RefCell::new(0usize);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut start = pending_start;
    let mut poll = |_d: &Path| PendingPoll::Resolving;
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {};
    let outcome = run_bg(
        reach_focus_and_launch_terminal(),
        sample_state(),
        &mut open,
        &mut preview,
        &mut start,
        &mut poll,
        &mut activate,
        &mut clear,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*activated.borrow(), 0, "a resolving launch is not moved to");
}

#[test]
fn acting_before_the_tab_is_ready_cancels_the_move() {
    // A keypress (↓) after the launch bumps the interaction epoch, so the loop drops
    // the tracker (and the in-flight launch) without moving to the tab.
    let activated = RefCell::new(0usize);
    let cleared = RefCell::new(0usize);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut start = pending_start;
    let mut poll = |_d: &Path| PendingPoll::Resolving;
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {
        *cleared.borrow_mut() += 1;
    };
    let mut keys = reach_focus_and_launch_terminal();
    keys.push(Ok(Key::ArrowDown)); // acts while loading -> cancels the auto-move
    let outcome = run_bg(
        keys,
        sample_state(),
        &mut open,
        &mut preview,
        &mut start,
        &mut poll,
        &mut activate,
        &mut clear,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*activated.borrow(), 0, "acting cancelled the move");
    assert!(*cleared.borrow() >= 1, "the in-flight launch was dropped");
}

#[test]
fn a_vanished_background_launch_is_dropped() {
    // The launch reports gone (spawn failed / pane vanished): the loop drops the
    // tracker without attaching.
    let activated = RefCell::new(0usize);
    let cleared = RefCell::new(0usize);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut start = pending_start;
    let mut poll = |_d: &Path| PendingPoll::Gone;
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {
        *cleared.borrow_mut() += 1;
    };
    let outcome = run_bg(
        reach_focus_and_launch_terminal(),
        sample_state(),
        &mut open,
        &mut preview,
        &mut start,
        &mut poll,
        &mut activate,
        &mut clear,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*activated.borrow(), 0, "a gone launch is never activated");
    assert!(*cleared.borrow() >= 1, "the gone launch was dropped");
}

#[test]
fn reusing_an_agent_tab_re_attaches_without_a_loading_tab() {
    // `start_pending_spawn` reporting `Reused` means no new tab: the launch just
    // re-attaches the existing pane, so no poll / activate happens.
    let activated = RefCell::new(0usize);
    let opens = RefCell::new(0usize);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opens.borrow_mut() += 1;
        if *opens.borrow() == 1 {
            Ok(PaneExit::ToFocus)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut start = |_: &mut HomeState, _: &Path, _: bool| Ok(StartPending::Reused);
    let mut poll = |_d: &Path| PendingPoll::Ready;
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {};
    let outcome = run_bg(
        reach_focus_and_launch_terminal(),
        sample_state(),
        &mut open,
        &mut preview,
        &mut start,
        &mut poll,
        &mut activate,
        &mut clear,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *activated.borrow(),
        0,
        "a reused tab tracks nothing to activate"
    );
    assert_eq!(
        *opens.borrow(),
        2,
        "the reuse re-attached (initial + reused)"
    );
}

#[test]
fn a_failed_dispatch_is_logged_and_tracks_nothing() {
    // When `start_pending_spawn` errors, the launch is logged and no pending tab is
    // tracked — the loop never polls or activates anything.
    let activated = RefCell::new(0usize);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| Ok(PaneExit::ToFocus);
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut start = |_: &mut HomeState, _: &Path, _: bool| Err(anyhow::anyhow!("no shell"));
    let mut poll = |_d: &Path| PendingPoll::Ready;
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {};
    let outcome = run_bg(
        reach_focus_and_launch_terminal(),
        sample_state(),
        &mut open,
        &mut preview,
        &mut start,
        &mut poll,
        &mut activate,
        &mut clear,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *activated.borrow(),
        0,
        "a failed dispatch tracks no pending tab"
    );
}
