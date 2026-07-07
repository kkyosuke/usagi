use super::*;

#[test]
fn ctrl_caret_in_overview_with_no_previous_session_is_a_no_op() {
    // Nothing has been focused yet, so the jump finds no target and the loop just
    // quits on the trailing Ctrl-C without ever attaching a pane.
    let dirs = run_capturing_attached_dirs(vec![Ok(Key::Char(CTRL_CARET)), Ok(Key::CtrlC)]);
    assert!(dirs.is_empty());
}

#[test]
fn ctrl_caret_on_the_base_overview_jumps_back_to_the_previous_session() {
    // Closeup feat, then main; Ctrl-^ from the base 選択 re-attaches feat.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter)); // attach feat -> Closeup
    keys.push(Ok(Key::Escape)); // -> Overview
    keys.extend(cmd("session switch main"));
    keys.push(Ok(Key::Enter)); // attach main (previous = feat) -> Closeup
    keys.push(Ok(Key::Escape)); // -> Overview
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
fn ctrl_caret_in_overview_jumps_back_to_the_previous_session() {
    // Same setup, but the jump is issued from 選択 (reached via Ctrl-O from Closeup).
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    keys.extend(cmd("session switch main"));
    keys.push(Ok(Key::Enter)); // Closeup main (previous = feat)
    keys.push(Ok(Key::Char(CTRL_O))); // Closeup -> Overview
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
fn ctrl_caret_in_closeup_jumps_back_to_the_previous_session() {
    // The jump is issued directly from 集中 (Closeup) on the current session.
    let mut keys = cmd("session switch feat");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    keys.extend(cmd("session switch main"));
    keys.push(Ok(Key::Enter)); // Closeup main (previous = feat)
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
    keys.push(Ok(Key::Enter)); // attach feat -> Closeup
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
fn ctrl_caret_from_an_attached_pane_with_no_previous_falls_back_to_closeup() {
    // Attaching the root (the first focus, recording no previous) and handing back
    // ToPreviousSession finds no target, so the pane drops to 集中 — exactly one
    // attach, no re-rooting.
    let opened = RefCell::new(Vec::new());
    let mut open = |_h: &mut HomeState, d: &Path, _a: bool, _n: bool| {
        opened.borrow_mut().push(d.to_path_buf());
        Ok(PaneExit::ToPreviousSession)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    // The root previews live (`live_preview`), so focusing it from Overview attaches
    // its pane directly — the first focus, recording no previous.
    let keys = vec![Ok(Key::Enter)]; // focus + attach root -> ToPreviousSession -> no target -> Closeup
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
    keys.push(Ok(Key::Enter)); // attach main again -> Closed -> Closeup
    keys.push(Ok(Key::Char(CTRL_Q))); // raise the modal again from Closeup
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
