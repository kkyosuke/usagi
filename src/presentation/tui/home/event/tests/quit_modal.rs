use super::*;

#[test]
fn ctrl_c_quits_outright_when_no_session_is_live() {
    // The default `run` harness has no live session, so Ctrl-C closes the app
    // without asking — the gate only triggers when something is running.
    assert!(matches!(
        run(vec![Ok(Key::CtrlC)], sample_state()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_q_at_the_base_overview_confirms_before_quitting() {
    // Unlike Ctrl-C, Ctrl-Q always raises the quit-confirmation modal first —
    // even with nothing live — so a lone Ctrl-Q does not quit; `y` then confirms.
    assert!(matches!(
        run(
            vec![Ok(Key::Char(CTRL_Q)), Ok(Key::Char('y'))],
            sample_state()
        )
        .unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_q_modal_can_be_cancelled_then_re_raised_and_confirmed() {
    // Ctrl-Q raises the modal on an idle screen; `n` cancels back to 選択 (proving
    // it did not quit, since the loop reads on); a second Ctrl-Q raises it again
    // and a third Ctrl-Q inside the modal confirms the close.
    let keys = vec![
        Ok(Key::Char(CTRL_Q)), // raise the modal (idle)
        Ok(Key::Char('n')),    // cancel -> 選択
        Ok(Key::Char(CTRL_Q)), // raise again
        Ok(Key::Char(CTRL_Q)), // confirm via a second Ctrl-Q inside the modal
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn ctrl_q_with_a_live_session_confirms_before_quitting() {
    // With a live session Ctrl-Q raises the same confirm modal; `y` confirms.
    let mut persist: fn(&str) = noop_persist;
    assert!(matches!(
        run_with_live_monitor(
            vec![Ok(Key::Char(CTRL_Q)), Ok(Key::Char('y'))],
            sample_state(),
            &mut persist,
        )
        .unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_c_with_a_live_session_quits_only_after_confirming() {
    // 'y' confirms the close.
    let mut persist: fn(&str) = noop_persist;
    assert!(matches!(
        run_with_live_monitor(
            vec![Ok(Key::CtrlC), Ok(Key::Char('y'))],
            sample_state(),
            &mut persist,
        )
        .unwrap(),
        Outcome::Quit
    ));

    // A second Ctrl-C inside the modal confirms too.
    assert!(matches!(
        run_with_live_monitor(
            vec![Ok(Key::CtrlC), Ok(Key::CtrlC)],
            sample_state(),
            &mut persist,
        )
        .unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn confirm_modal_cancel_keeps_the_screen_running() {
    // Ctrl-C raises the modal (a session is live); an ignored key is a no-op
    // in it; 'n' cancels back to 選択, where a palette command still runs (proving
    // the first Ctrl-C did not quit). Esc also cancels; Enter finally confirms.
    let mut keys = vec![
        Ok(Key::CtrlC),     // raise the modal
        Ok(Key::Home),      // ignored inside the modal
        Ok(Key::Char('n')), // cancel -> 選択
    ];
    keys.extend(cmd("man")); // open the palette and type `man`
    keys.push(Ok(Key::Enter)); // runs `man` -> persisted (opens a text modal)
    keys.push(Ok(Key::Escape)); // dismiss the text modal
    keys.push(Ok(Key::Escape)); // close the palette
    keys.push(Ok(Key::CtrlC)); // raise again
    keys.push(Ok(Key::Escape)); // cancel via Esc
    keys.push(Ok(Key::CtrlC)); // raise again
    keys.push(Ok(Key::Enter)); // confirm via Enter -> quit

    let mut recorded = Vec::new();
    let mut persist = |c: &str| recorded.push(c.to_string());
    let outcome = run_with_live_monitor(keys, sample_state(), &mut persist).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // The command ran between the cancelled closes, so the screen kept going.
    assert_eq!(recorded, vec!["man"]);
}

#[test]
fn an_interrupted_blocking_read_is_retried_not_treated_as_quit() {
    // With no live session the loop blocks in `read_key`. A delivered signal
    // (`EINTR`) interrupting that read must not quit: before the fix the loop
    // returned `Outcome::Quit`, which dropped the user out of the alternate
    // screen and exposed the pre-launch scrollback. It now re-reads, so the
    // typed command still runs before the trailing Ctrl-C quits.
    let created = RefCell::new(Vec::<String>::new());
    let mut create = |name: &str| {
        created.borrow_mut().push(name.to_string());
        noop_create(name)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys: Vec<io::Result<Key>> = vec![Err(io::Error::from(io::ErrorKind::Interrupted))];
    keys.extend(cmd("session create foo"));
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::CtrlC));
    let outcome = run_full(
        keys,
        sample_state(),
        &mut open,
        &mut create,
        &mut preview,
        &mut noop_config,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*created.borrow(), vec!["foo".to_string()]);
}

#[test]
fn an_interrupted_animate_read_is_retried_not_treated_as_quit() {
    // While a session is live the loop waits in `read_key_timeout` (the animate
    // path) — the exact case the user hit: exit an agent, then `Ctrl-O` while
    // waiting, and a signal interrupting the wait quit the app. The interrupted
    // read is now swallowed, the typed command still runs, and only the
    // confirmed Ctrl-C quits.
    let mut keys: Vec<io::Result<Key>> = vec![Err(io::Error::from(io::ErrorKind::Interrupted))];
    keys.extend(cmd("man"));
    keys.push(Ok(Key::Enter)); // runs `man` -> persisted (text modal over the palette)
    keys.push(Ok(Key::Escape)); // dismiss the text modal
    keys.push(Ok(Key::CtrlC)); // raise the quit modal (a session is live)
    keys.push(Ok(Key::Enter)); // confirm -> quit
    let mut recorded = Vec::new();
    let mut persist = |c: &str| recorded.push(c.to_string());
    let outcome = run_with_live_monitor(keys, sample_state(), &mut persist).unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    // `man` ran after the interrupted read, proving it was not treated as quit.
    assert_eq!(recorded, vec!["man"]);
}
