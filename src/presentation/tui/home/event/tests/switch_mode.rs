use super::*;

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

#[test]
fn cheatsheet_opens_from_the_base_switch_and_dismisses() {
    // `?` at the base 切替 opens the keybinding cheat sheet (a scrollable text
    // modal); the arrows / j/k scroll it and Esc dismisses it (back to Switch,
    // where the trailing Ctrl-C quits).
    let keys = vec![
        Ok(Key::Char('?')), // open the cheat sheet
        Ok(Key::ArrowDown), // scroll down a line
        Ok(Key::Char('j')),
        Ok(Key::ArrowUp), // scroll up a line
        Ok(Key::Char('k')),
        Ok(Key::Escape), // dismiss -> Switch
        Ok(Key::CtrlC),  // nothing live: quits outright
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}
