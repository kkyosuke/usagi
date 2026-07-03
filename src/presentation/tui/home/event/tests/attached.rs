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
    // alive *without* re-attaching: the pane opens exactly once. ToFocus arms a
    // one-shot return-to-pane (the next Esc would re-attach), but a deliberate key
    // (here ↓ in the menu) cancels it, so the following Esc peels back to Switch
    // instead (then Esc is inert; fallback Ctrl-C quits).
    let calls = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *calls.borrow_mut() += 1;
        Ok(PaneExit::ToFocus)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // Focus root -> attach -> ToFocus -> Focus (arm return)
    keys.push(Ok(Key::ArrowDown)); // a menu move cancels the one-shot return arming
    keys.push(Ok(Key::Escape)); // Focus -> Switch (no longer armed)
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
fn ctrl_t_then_esc_re_attaches_to_the_zoomed_out_pane() {
    // The reported flow: attach an agent, zoom out with Ctrl-T / Ctrl-O a (ToFocus)
    // to the 在席 action menu, then Esc. The immediate Esc must return to the pane
    // the zoom started from (没入) rather than landing in Focus. A live tab strip is
    // republished each frame (as in production), so `focus_discard_new_tab` *would*
    // fire on Esc — the one-shot return arming has to win. The pane returns ToFocus
    // first, then Closed on re-attach so the run ends; it opens exactly twice (the
    // initial attach and the Esc re-attach).
    let calls = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        let mut n = calls.borrow_mut();
        *n += 1;
        if *n == 1 {
            Ok(PaneExit::ToFocus)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut tab_op = |_: &Path, _: Option<TabNav>| (vec!["agent".to_string()], 0usize);
    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // attach -> ToFocus -> 在席 (arm return)
    keys.push(Ok(Key::Escape)); // Esc -> re-attach -> Closed -> 在席
    keys.push(Ok(Key::Escape)); // 在席 over the live strip -> discard new tab (preview)
    keys.push(Ok(Key::Escape)); // 在席 -> 切替
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits
    assert!(matches!(
        run_full_tabs(keys, sample_state(), &mut open, &mut preview, &mut tab_op).unwrap(),
        Outcome::Quit
    ));
    assert_eq!(*calls.borrow(), 2);
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

#[test]
fn a_double_click_in_an_attached_pane_switches_to_the_clicked_session() {
    // From 没入, a sidebar double click surfaces as PaneExit::ToSession(row):
    // attaching `feat` hands it back targeting focus row 1 (`main`), so the loop
    // re-roots on `main` (re-attaching it), then `main` closes and drops to 在席.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, _a: bool, _n: bool| {
        opened.borrow_mut().push(d.to_path_buf());
        if d.ends_with("feat") {
            Ok(PaneExit::ToSession(1)) // focus row 1 -> `main`
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat -> ToSession(1) -> re-attach main -> Closed
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
        vec![PathBuf::from("/r/feat"), PathBuf::from("/r/main")],
    );
}

#[test]
fn a_double_click_on_the_create_row_in_an_attached_pane_opens_inline_create() {
    // From 没入, the pane reports a double click on the sidebar create row as
    // PaneExit::ToSession(create_row). The event loop leaves the pane alive,
    // opens the same inline create editor used by 切替 / 在席, and the typed name
    // is dispatched through the normal create callback.
    let state = sample_state();
    let create_row = state.list().create_row();
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, _a: bool, _n: bool| {
        opened.borrow_mut().push(d.to_path_buf());
        Ok(PaneExit::ToSession(create_row))
    };
    let created = RefCell::new(Vec::new());
    let mut create = |name: &str| {
        created.borrow_mut().push(name.to_string());
        noop_create(name)
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat -> ToSession(create_row) -> inline create
    keys.extend(typed("wip"));
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
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
    assert_eq!(*opened.borrow(), vec![PathBuf::from("/r/feat")]);
    assert_eq!(*created.borrow(), vec!["wip"]);
}
