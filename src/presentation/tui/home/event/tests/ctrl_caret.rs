use super::*;

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
