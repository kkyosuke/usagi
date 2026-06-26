use super::*;

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
    // Dispatched without force (`false`): a dirty session is refused, not discarded.
    assert_eq!(removed, vec![("feat".to_string(), false)]);
    assert_eq!(
        branches_called, 1,
        "`c` after close began inline create, so the screen is in 切替 (Switch)"
    );
}

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
