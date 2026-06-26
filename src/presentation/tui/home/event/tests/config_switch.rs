use super::*;

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
    assert!(
        tab_dirs.borrow().iter().any(|d| d == Path::new("/r/feat")),
        "the attached session's surface must be re-published while the note editor floats over it",
    );
}
