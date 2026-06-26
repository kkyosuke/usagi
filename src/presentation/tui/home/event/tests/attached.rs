use super::*;

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
