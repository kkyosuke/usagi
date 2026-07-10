//! The background loading-tab flow: 集中's `terminal` / `agent` on a session that
//! already shows tabs dispatches the pane through `start_pending_spawn` (no attach,
//! no centre loader), selects the loading tab in the strip, and the loop attaches
//! it once ready if it is still selected. These drive the loop with capturing
//! background hooks (the pool-less [`super::event_loop_compat`] only no-ops them).
//! Each test pins one loop branch: the fake `poll_pending_spawn` returns a fixed
//! phase, since [`ScriptedReader`] cancels on the next key.

use super::*;
use std::cell::RefCell;

/// Drive the loop with capturing background-pane hooks. Reaching 集中 on a live
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
    // A live tab strip so 集中's `terminal` reads the session as already showing
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
        autostart_queued: &mut autostart_queued,
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
        broadcast_wake: &mut broadcast_wake,
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

/// Keys that reach 集中 on live `feat` and run its `terminal` command: switch to
/// feat, `Enter` to re-attach (zooming out to 集中 via `ToCloseup`), then `t`.
fn reach_closeup_and_launch_terminal() -> Vec<io::Result<Key>> {
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // re-attach live feat -> ToCloseup -> 集中 menu
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
            Ok(PaneExit::ToFocus) // the initial re-attach zooms out to 集中
        } else {
            Ok(PaneExit::Closed) // the ready pending tab, once attached
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut start = pending_start;
    let mut poll = |_d: &Path| PendingPoll::Ready { selected: true };
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {};
    let outcome = run_bg(
        reach_closeup_and_launch_terminal(),
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
    assert_eq!(*activated.borrow(), 1, "the ready pane was re-asserted");
    // Two attaches: the initial re-attach, then the now-active background tab.
    assert_eq!(*opens.borrow(), vec![(false, false), (false, false)]);
}

#[test]
fn a_ready_tab_that_is_no_longer_selected_stays_in_the_background() {
    // Readiness no longer performs a delayed tab selection. If the pool reports
    // that the pending pane is no longer the selected tab, the loop just drops the
    // tracker and leaves the spawned pane as a normal background tab.
    let activated = RefCell::new(0usize);
    let cleared = RefCell::new(0usize);
    let opens = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, _d: &Path, a: bool, n: bool| {
        opens.borrow_mut().push((a, n));
        Ok(PaneExit::ToFocus)
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut start = pending_start;
    let mut poll = |_d: &Path| PendingPoll::Ready { selected: false };
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {
        *cleared.borrow_mut() += 1;
    };
    let outcome = run_bg(
        reach_closeup_and_launch_terminal(),
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
    assert_eq!(*activated.borrow(), 0, "no ready-time tab selection");
    assert_eq!(*cleared.borrow(), 1, "the launch tracker is consumed");
    assert_eq!(
        *opens.borrow(),
        vec![(false, false)],
        "only the initial re-attach ran"
    );
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
        reach_closeup_and_launch_terminal(),
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
        reach_closeup_and_launch_terminal(),
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
fn acting_before_the_tab_is_ready_keeps_the_launch_pending() {
    // A keypress (↓) after the launch no longer cancels the pending tab: selection
    // moved to the tab at dispatch time, so a still-resolving launch remains in
    // flight and simply has not attached yet.
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
    let mut keys = reach_closeup_and_launch_terminal();
    keys.push(Ok(Key::ArrowDown)); // acts while loading; the launch keeps running
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
    assert_eq!(
        *activated.borrow(),
        0,
        "still resolving, so nothing attached"
    );
    assert_eq!(*cleared.borrow(), 0, "the in-flight launch keeps running");
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
        reach_closeup_and_launch_terminal(),
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
    let mut poll = |_d: &Path| PendingPoll::Ready { selected: true };
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {};
    let outcome = run_bg(
        reach_closeup_and_launch_terminal(),
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
    let mut poll = |_d: &Path| PendingPoll::Ready { selected: true };
    let mut activate = |_d: &Path| {
        *activated.borrow_mut() += 1;
        true
    };
    let mut clear = || {};
    let outcome = run_bg(
        reach_closeup_and_launch_terminal(),
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

#[derive(Clone, Copy, Debug)]
enum PendingCase {
    Resolving,
    Starting(usize),
    ReadySelected,
    ReadyBackground,
    Gone,
}

impl PendingCase {
    fn poll(self) -> PendingPoll {
        match self {
            Self::Resolving => PendingPoll::Resolving,
            Self::Starting(index) => PendingPoll::Starting(index),
            Self::ReadySelected => PendingPoll::Ready { selected: true },
            Self::ReadyBackground => PendingPoll::Ready { selected: false },
            Self::Gone => PendingPoll::Gone,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum LaunchCase {
    Pending(PendingCase),
    Reused,
    Error,
}

#[test]
fn background_tab_launch_lifecycle_characterization_matrix() {
    // A compact matrix for the background-tab lifecycle. Every row starts from a
    // live session, zooms out to 集中, and runs `terminal`; the first open is that
    // initial re-attach, and later counts show what the launch path did.
    //
    // start_pending_spawn / poll result -> (open count, activate count, clear count)
    //
    // - Pending + Ready(selected) attaches the newly ready tab.
    // - Pending + Ready(background) and Gone consume the tracker without opening.
    // - Pending + Resolving/Starting leaves the tracker alive.
    // - Reused re-attaches immediately with no tracker.
    // - Error logs and tracks nothing.
    let cases = [
        (
            "pending resolving",
            LaunchCase::Pending(PendingCase::Resolving),
            1,
            0,
            0,
        ),
        (
            "pending starting",
            LaunchCase::Pending(PendingCase::Starting(1)),
            1,
            0,
            0,
        ),
        (
            "pending ready selected",
            LaunchCase::Pending(PendingCase::ReadySelected),
            2,
            1,
            0,
        ),
        (
            "pending ready background",
            LaunchCase::Pending(PendingCase::ReadyBackground),
            1,
            0,
            1,
        ),
        (
            "pending gone",
            LaunchCase::Pending(PendingCase::Gone),
            1,
            0,
            1,
        ),
        ("reused", LaunchCase::Reused, 2, 0, 0),
        ("error", LaunchCase::Error, 1, 0, 0),
    ];

    for (name, launch, expect_opens, expect_activates, expect_clears) in cases {
        let activated = RefCell::new(0usize);
        let cleared = RefCell::new(0usize);
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
        let mut start = move |_: &mut HomeState, _: &Path, _: bool| match launch {
            LaunchCase::Pending(_) => Ok(StartPending::Pending {
                label: "terminal".to_string(),
            }),
            LaunchCase::Reused => Ok(StartPending::Reused),
            LaunchCase::Error => Err(anyhow::anyhow!("no shell")),
        };
        let mut poll = move |_d: &Path| match launch {
            LaunchCase::Pending(case) => case.poll(),
            LaunchCase::Reused | LaunchCase::Error => PendingPoll::Ready { selected: true },
        };
        let mut activate = |_d: &Path| {
            *activated.borrow_mut() += 1;
            true
        };
        let mut clear = || {
            *cleared.borrow_mut() += 1;
        };

        let outcome = run_bg(
            reach_closeup_and_launch_terminal(),
            sample_state(),
            &mut open,
            &mut preview,
            &mut start,
            &mut poll,
            &mut activate,
            &mut clear,
        )
        .unwrap();

        assert!(matches!(outcome, Outcome::Quit), "outcome: {name}");
        assert_eq!(*opens.borrow(), expect_opens, "open count: {name}");
        assert_eq!(
            *activated.borrow(),
            expect_activates,
            "activate count: {name}"
        );
        assert_eq!(*cleared.borrow(), expect_clears, "clear count: {name}");
    }
}
