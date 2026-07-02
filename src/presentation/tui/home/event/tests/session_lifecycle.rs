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
fn typing_on_the_visible_create_row_creates_a_session() {
    // The persistent "+ new session" row is a real keyboard target: once the
    // cursor rests on it, the first printable character opens the inline editor
    // and becomes the first character of the new session name.
    let mut keys = vec![
        Ok(Key::ArrowDown), // root -> main
        Ok(Key::ArrowDown), // main -> "+ new session"
    ];
    keys.extend(typed("wip"));
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));

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
            state_with_sessions(&["main"]),
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
fn enter_on_the_visible_create_row_opens_the_empty_create_input() {
    // Pressing Enter on the persistent create row opens the input without
    // pre-seeding a character; the following typed name is created as-is.
    let mut keys = vec![
        Ok(Key::ArrowDown), // root -> main
        Ok(Key::ArrowDown), // main -> "+ new session"
        Ok(Key::Enter),     // open empty create input
    ];
    keys.extend(typed("wip"));
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));

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
            state_with_sessions(&["main"]),
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
fn create_row_keeps_arrow_escape_and_ignored_key_paths() {
    // The create row is a navigation target, but still keeps non-typing escape
    // hatches: arrows move away / back, Esc is handled, and an unrelated special
    // key is ignored without opening the input.
    let keys = vec![
        Ok(Key::ArrowDown), // root -> main
        Ok(Key::ArrowDown), // main -> "+ new session"
        Ok(Key::ArrowUp),   // create -> main
        Ok(Key::ArrowDown), // main -> create
        Ok(Key::ArrowDown), // create -> root
        Ok(Key::ArrowUp),   // root -> create
        Ok(Key::ArrowLeft), // ignored on create row
        Ok(Key::Escape),    // handled on create row (base Switch stays put)
        Ok(Key::CtrlC),
    ];
    assert!(matches!(
        run(keys, state_with_sessions(&["main"])).unwrap(),
        Outcome::Quit
    ));
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
                last_active: None,
            },
            SessionRecord {
                name: name.to_string(),
                display_name: None,
                note: None,
                root: PathBuf::from("/ws/.usagi/sessions/newx"),
                worktrees: vec![worktree(Some(name), "/r/newx")],
                created_at: Utc::now(),
                last_active: None,
            },
        ]),
        select: None,
        root_note: None,
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
fn finished_create_does_not_auto_focus_after_another_operation() {
    use super::super::super::tasks::{AutoFocus, Completion, TaskKind};
    use std::rc::Rc;

    struct CompleteOnArrowDown {
        keys: VecDeque<io::Result<Key>>,
        tasks: TaskHandle,
        task_id: Rc<RefCell<Option<u64>>>,
        focus: Rc<RefCell<Option<AutoFocus>>>,
    }

    impl KeyReader for CompleteOnArrowDown {
        fn read_key(&mut self) -> io::Result<Key> {
            let key = self.keys.pop_front().unwrap_or(Ok(Key::CtrlC))?;
            if matches!(key, Key::ArrowDown) {
                let task_id = self.task_id.borrow_mut().take();
                let focus = self.focus.borrow_mut().take();
                if let (Some(id), Some(focus)) = (task_id, focus) {
                    self.tasks.complete(
                        id,
                        true,
                        Completion {
                            line: LogLine::output("created"),
                            sessions: Some(vec![
                                SessionRecord {
                                    name: "main".to_string(),
                                    display_name: None,
                                    note: None,
                                    root: PathBuf::from("/ws/.usagi/sessions/main"),
                                    worktrees: vec![worktree(Some("main"), "/r/main")],
                                    created_at: Utc::now(),
                                    last_active: None,
                                },
                                SessionRecord {
                                    name: "feat".to_string(),
                                    display_name: None,
                                    note: None,
                                    root: PathBuf::from("/ws/.usagi/sessions/feat"),
                                    worktrees: vec![worktree(Some("feat"), "/r/feat")],
                                    created_at: Utc::now(),
                                    last_active: None,
                                },
                                SessionRecord {
                                    name: "newx".to_string(),
                                    display_name: None,
                                    note: None,
                                    root: PathBuf::from("/ws/.usagi/sessions/newx"),
                                    worktrees: vec![worktree(Some("newx"), "/r/newx")],
                                    created_at: Utc::now(),
                                    last_active: None,
                                },
                            ]),
                            target_root: Some(PathBuf::from("/ws")),
                            evict: None,
                            focus: Some(focus),
                        },
                    );
                }
            }
            Ok(key)
        }
    }

    let mut keys = cmd("session create newx");
    keys.push(Ok(Key::Enter)); // dispatch create, but leave the task running
    keys.push(Ok(Key::ArrowDown)); // another user operation before completion lands
    keys.push(Ok(Key::Char('t'))); // Switch-only: focus the selected existing row
    keys.push(Ok(Key::Char('t'))); // Focus menu: open terminal on that row

    let tasks = TaskHandle::new();
    let task_id = Rc::new(RefCell::new(None));
    let focus = Rc::new(RefCell::new(None));
    let mut reader = CompleteOnArrowDown {
        keys: keys.into(),
        tasks: tasks.clone(),
        task_id: task_id.clone(),
        focus: focus.clone(),
    };
    let term = Term::stdout();
    let monitor = MonitorHandle::detached();
    let opened = RefCell::new(Vec::new());
    let mut open = |_: &mut HomeState, dir: &Path, _: bool, _: bool| {
        opened.borrow_mut().push(dir.to_path_buf());
        Ok(PaneExit::Closed)
    };
    let mut persist: fn(&crate::domain::history::HistoryEntry) = noop_persist_entry;
    let mut dispatch_create = |_: &Path, name: &str, interaction_epoch: u64| {
        let id = tasks.begin(TaskKind::CreateSession, name);
        *task_id.borrow_mut() = Some(id);
        *focus.borrow_mut() = Some(AutoFocus {
            name: name.to_string(),
            interaction_epoch,
        });
    };
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut remove = |_: &Path, _: &str, _: bool, _| {};
    let mut unite_resolve: fn(&str) -> std::result::Result<GroupSource, String> = no_unite_resolve;
    let mut update = || {};
    let mut evict = |_: &Path| {};
    let mut branches: fn() -> Vec<String> = no_branches;
    let mut open_url: fn(&str) = noop_open_url;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions: false,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        open_url: &mut open_url,
        open_config: &mut config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
    };

    assert!(matches!(
        event_loop(
            &term,
            &mut reader,
            sample_state(),
            &monitor,
            &UpdateHandle::new(),
            &SessionsRefreshHandle::new(),
            &OneShot::<bool>::new(),
            &OneShot::<Vec<AgentCli>>::new(),
            &tasks,
            &mut wiring,
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(
        opened.borrow().as_slice(),
        &[PathBuf::from("/ws/.usagi/sessions/main")]
    );
}

#[test]
fn finished_close_drops_into_focus_on_the_previous_session() {
    // `close` removes the focused session on a worker. When it finishes before
    // the user does anything else, the landing mirrors create's auto-focus path:
    // focus the nearest session above the closed one instead of snapping to root.
    // Prove that by pressing Focus menu's `t` shortcut after completion — it can
    // only open `/main` if the close landed in 在席 on `main`.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus(feat), menu UI
    keys.push(Ok(Key::ArrowUp)); // agent wraps up to close (last)
    keys.push(Ok(Key::Enter)); // dispatch close; completion drains next frame -> Focus(main)
    keys.push(Ok(Key::Char('t'))); // Focus menu shortcut: open terminal on `main`

    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let opened = RefCell::new(Vec::new());
    let mut open = |_: &mut HomeState, dir: &Path, _: bool, _: bool| {
        opened.borrow_mut().push(dir.to_path_buf());
        Ok(PaneExit::Closed)
    };
    let mut persist: fn(&str) = noop_persist;
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove = |name: &str, force: bool| {
        assert_eq!((name, force), ("feat", false));
        SessionOutcome {
            line: LogLine::output("removed"),
            sessions: Some(vec![SessionRecord {
                name: "main".to_string(),
                display_name: None,
                note: None,
                root: PathBuf::from("/ws/.usagi/sessions/main"),
                worktrees: vec![worktree(Some("main"), "/r/main")],
                created_at: Utc::now(),
                last_active: None,
            }]),
            select: None,
            root_note: None,
        }
    };
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    assert!(matches!(
        event_loop_compat(
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
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(
        opened.borrow().as_slice(),
        &[PathBuf::from("/ws/.usagi/sessions/main")]
    );
}

#[test]
fn finished_close_does_not_auto_focus_after_another_operation() {
    use super::super::super::tasks::{AutoFocus, Completion, TaskKind};
    use std::rc::Rc;

    struct CompleteRemoveOnArrowUp {
        keys: VecDeque<io::Result<Key>>,
        tasks: TaskHandle,
        task_id: Rc<RefCell<Option<u64>>>,
        focus: Rc<RefCell<Option<AutoFocus>>>,
    }

    impl KeyReader for CompleteRemoveOnArrowUp {
        fn read_key(&mut self) -> io::Result<Key> {
            let key = self.keys.pop_front().unwrap_or(Ok(Key::CtrlC))?;
            if matches!(key, Key::ArrowUp) {
                let task_id = self.task_id.borrow_mut().take();
                let focus = self.focus.borrow_mut().take();
                if let (Some(id), Some(focus)) = (task_id, focus) {
                    self.tasks.complete(
                        id,
                        true,
                        Completion {
                            line: LogLine::output("removed"),
                            sessions: Some(vec![SessionRecord {
                                name: "main".to_string(),
                                display_name: None,
                                note: None,
                                root: PathBuf::from("/ws/.usagi/sessions/main"),
                                worktrees: vec![worktree(Some("main"), "/r/main")],
                                created_at: Utc::now(),
                                last_active: None,
                            }]),
                            target_root: Some(PathBuf::from("/ws")),
                            evict: Some(PathBuf::from("/ws/.usagi/sessions/feat")),
                            focus: Some(focus),
                        },
                    );
                }
            }
            Ok(key)
        }
    }

    // Reach `close` (last) with ArrowDown, not ArrowUp: this reader completes the
    // remove task on ArrowUp, and that must only fire *after* close is dispatched.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus(feat), menu UI
    keys.push(Ok(Key::ArrowDown)); // agent -> terminal
    keys.push(Ok(Key::ArrowDown)); // terminal -> close (last)
    keys.push(Ok(Key::Enter)); // dispatch close, but leave the task running
    keys.push(Ok(Key::ArrowUp)); // another Switch operation before completion lands
    keys.push(Ok(Key::Char('c'))); // still Switch: begin inline create
    keys.push(Ok(Key::Escape)); // cancel create; reader then runs out -> quit

    let tasks = TaskHandle::new();
    let task_id = Rc::new(RefCell::new(None));
    let focus = Rc::new(RefCell::new(None));
    let mut reader = CompleteRemoveOnArrowUp {
        keys: keys.into(),
        tasks: tasks.clone(),
        task_id: task_id.clone(),
        focus: focus.clone(),
    };
    let term = Term::stdout();
    let monitor = MonitorHandle::detached();
    let mut persist: fn(&crate::domain::history::HistoryEntry) = noop_persist_entry;
    let mut dispatch_create = |_: &Path, _: &str, _: u64| {};
    let mut rename = |_: &Path, n: &str, l: &str| noop_rename(n, l);
    let mut set_note_fake = |_: &Path, n: &str, t: &str| noop_set_note(n, t);
    let mut reorder_fake: fn(&str, bool) -> SessionReorder = noop_reorder;
    let mut remove = |_: &Path, name: &str, _: bool, auto_focus: Option<AutoFocus>| {
        let id = tasks.begin(TaskKind::RemoveSession, name);
        *task_id.borrow_mut() = Some(id);
        *focus.borrow_mut() = auto_focus;
    };
    let mut unite_resolve: fn(&str) -> std::result::Result<GroupSource, String> = no_unite_resolve;
    let mut update = || {};
    let mut evict = |_: &Path| {};
    let mut branches_called = 0;
    let mut branches = || {
        branches_called += 1;
        Vec::new()
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut open_url: fn(&str) = noop_open_url;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut tab_op: fn(&Path, Option<TabNav>) -> (Vec<String>, usize) = noop_tab_op;
    let mut close: fn(&mut HomeState, &Path) = noop_close;
    let mut save_resume = |_: &str, _: ResumeLevel| {};
    let mut save_last_active = |_: &[(String, DateTime<Utc>)]| {};
    let mut tab_action = |_: &mut HomeState, _: &Path, _: usize, _: TabMenuAction| {};
    let mut wiring = Wiring {
        interaction_epoch: 0,
        watch_sessions: false,
        workspace_root: Path::new("/ws"),
        persist: &mut persist,
        dispatch_create: &mut dispatch_create,
        rename_display: &mut rename,
        set_note: &mut set_note_fake,
        reorder_session: &mut reorder_fake,
        dispatch_remove: &mut remove,
        unite_resolve: &mut unite_resolve,
        dispatch_update: &mut update,
        evict_pool: &mut evict,
        existing_branches: &mut branches,
        open_terminal: &mut open,
        open_url: &mut open_url,
        open_config: &mut config,
        preview: &mut preview,
        tab_op: &mut tab_op,
        close_tab: &mut close,
        tab_action: &mut tab_action,
        save_resume: &mut save_resume,
        save_last_active: &mut save_last_active,
    };

    assert!(matches!(
        event_loop(
            &term,
            &mut reader,
            sample_state(),
            &monitor,
            &UpdateHandle::new(),
            &SessionsRefreshHandle::new(),
            &OneShot::<bool>::new(),
            &OneShot::<Vec<AgentCli>>::new(),
            &tasks,
            &mut wiring,
        )
        .unwrap(),
        Outcome::Quit
    ));
    assert_eq!(
        branches_called, 1,
        "`c` after the delayed close completion stayed in 切替 instead of auto-focusing"
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
            root_note: None,
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
    // The 在席 menu lists its commands in the fixed order (`agent`, `terminal`,
    // `close`) with `agent` highlighted by default; `close` is last, so ArrowUp
    // wraps up to it. Enter removes the focused session like `session remove feat`
    // (no `--force`), then drops into 切替 (Switch) — the `c` keypress that follows
    // opens the inline create input (a Switch-only action), proving the landing mode.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus (feat), menu UI
    keys.push(Ok(Key::ArrowUp)); // agent wraps up to `close` (last)
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
            root_note: None,
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
fn focus_menu_shift_c_force_closes_the_focused_session_then_enters_switch() {
    // Shift+c (reported by `console` as capital `C`) is the 在席 menu's explicit
    // discard shortcut: it runs `close --force` without moving the menu cursor.
    // The screen still lands in 切替 (Switch), proven by the following `c`
    // beginning inline creation.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus (feat), menu UI
    keys.push(Ok(Key::Char('C'))); // run `close --force` -> 切替 (Switch)
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
            root_note: None,
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
    assert_eq!(removed, vec![("feat".to_string(), true)]);
    assert_eq!(
        branches_called, 1,
        "`c` after Shift+c close began inline create, so the screen is in 切替 (Switch)"
    );
}

#[test]
fn focus_menu_close_picker_enter_runs_plain_close() {
    // `→` on the close row opens the picker; `Enter` on option 0 runs plain `close`.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus (feat)
    keys.push(Ok(Key::ArrowUp)); // agent wraps up to close (last)
    keys.push(Ok(Key::ArrowRight)); // open close picker (option 0 = close)
    keys.push(Ok(Key::Enter)); // run `close` -> 切替
    keys.push(Ok(Key::Char('c'))); // Switch-only: begin inline create
    keys.push(Ok(Key::Escape)); // cancel; reader runs out -> quit
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
            root_note: None,
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
    assert_eq!(removed, vec![("feat".to_string(), false)]);
    assert_eq!(branches_called, 1, "landed in 切替 after plain close");
}

#[test]
fn focus_menu_close_picker_enter_on_force_runs_force_close() {
    // `→` opens the picker; `↓` selects `--force`; `Enter` runs `close --force`.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus (feat)
    keys.push(Ok(Key::ArrowUp)); // agent wraps up to close (last)
    keys.push(Ok(Key::ArrowRight)); // open close picker
    keys.push(Ok(Key::ArrowDown)); // option 0 -> option 1 (close --force)
    keys.push(Ok(Key::Enter)); // run `close --force` -> 切替
    keys.push(Ok(Key::Char('c'))); // Switch-only: begin inline create
    keys.push(Ok(Key::Escape)); // cancel; reader runs out -> quit
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
            root_note: None,
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
    assert_eq!(removed, vec![("feat".to_string(), true)]);
    assert_eq!(branches_called, 1, "landed in 切替 after close --force");
}

/// Minimal event-loop harness for close-picker navigation tests.
fn run_close_picker_keys(extra_keys: Vec<io::Result<Key>>) -> (Outcome, Vec<(String, bool)>) {
    // Start: navigate to close row, open picker, then apply caller's keys.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // -> Focus (feat)
    keys.push(Ok(Key::ArrowUp)); // agent wraps up to close (last)
    keys.push(Ok(Key::ArrowRight)); // open close picker
    keys.extend(extra_keys);
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
            root_note: None,
        }
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut config: fn(&Term) -> Result<Option<ConfigReload>> = noop_config;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut branches = || Vec::new();
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
    (outcome, removed)
}

#[test]
fn focus_menu_close_picker_left_collapses_without_executing() {
    // `←` collapses the picker back to the menu without running any command.
    // An unhandled key (`t`) while the picker is open is also a no-op (catch-all arm).
    let (outcome, removed) = run_close_picker_keys(vec![
        Ok(Key::Char('t')), // no-op: unhandled key inside close picker
        Ok(Key::ArrowLeft), // collapse picker
        Ok(Key::Escape),    // leave Focus -> 切替
        Ok(Key::Escape),    // quit
    ]);
    assert!(matches!(outcome, Outcome::Quit));
    assert!(removed.is_empty(), "← must not execute close");
}

#[test]
fn focus_menu_close_picker_up_wraps_to_force_and_runs_it() {
    // `↑` from option 0 (close) wraps to option 1 (close --force); `Enter` runs it.
    let (outcome, removed) = run_close_picker_keys(vec![
        Ok(Key::ArrowUp),   // 0 -> 1 (close --force), exercises move_up close_cursor path
        Ok(Key::Enter),     // run close --force
        Ok(Key::Char('c')), // Switch-only: begin inline create -> proves 切替
        Ok(Key::Escape),    // cancel; reader runs out -> quit
    ]);
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(removed, vec![("feat".to_string(), true)]);
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
    keys.push(Ok(Key::Char('y'))); // confirm via the yes-key (same as Enter)
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
    keys.push(Ok(Key::Char('Y'))); // nothing checked (yes-key) -> also stays open
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
