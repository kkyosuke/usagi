use super::*;

#[test]
fn escape_at_the_base_switch_is_inert_and_does_not_leave() {
    // Esc no longer backs out to the project list: it is a no-op at the base
    // 切替 (the default), so the loop runs on and only the fallback Ctrl-C (no
    // live session) quits. A Back-returning Esc would instead resolve to
    // `Outcome::Back` here.
    assert!(matches!(
        run(vec![Ok(Key::Escape)], sample_state()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_c_at_the_base_switch_returns_quit() {
    assert!(matches!(
        run(vec![Ok(Key::CtrlC)], sample_state()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn ctrl_b_toggles_the_sidebar_and_keeps_the_screen_running() {
    // Ctrl-B is a view-only sidebar toggle handled before the per-mode dispatch:
    // the loop collapses / expands the sidebar and keeps running, so the
    // following Ctrl-C still quits. Two presses exercise both directions.
    let keys = vec![
        Ok(Key::Char(CTRL_B)), // Full -> Rail
        Ok(Key::Char(CTRL_B)), // Rail -> Full
        Ok(Key::CtrlC),        // still running -> quit
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn colon_opens_the_command_palette_from_the_base_switch() {
    // `:` at the base 切替 summons the command palette overlay; `Esc` closes it
    // back to 切替, where Esc is inert and the fallback Ctrl-C quits.
    let keys = vec![
        Ok(Key::Char(':')), // base Switch -> command palette
        Ok(Key::Escape),    // close the palette -> base Switch
        Ok(Key::Escape),    // Esc inert at the base Switch; fallback Ctrl-C quits
    ];
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn escape_in_switch_closes_the_note_before_backing_out() {
    // With the highlighted session's read-only note showing, the first Esc closes
    // the note and stays in 切替; a second Esc is then inert at the base 切替, and
    // the fallback Ctrl-C quits. The note's lifecycle is owned by Esc before the
    // mode's is. 切替 is the default, so no Ctrl-O is needed to reach it.
    let mut state = sample_state();
    state.restore_sessions(vec![SessionRecord {
        name: "alpha".to_string(),
        display_name: None,
        note: Some("todo".to_string()),
        root: PathBuf::from("/ws/.usagi/sessions/alpha"),
        worktrees: vec![worktree(Some("alpha"), "/ws/alpha")],
        created_at: Utc::now(),
        last_active: None,
    }]);
    let keys = vec![
        Ok(Key::ArrowDown), // root -> alpha; its note auto-shows
        Ok(Key::Escape),    // close the note (stays in Switch)
        Ok(Key::Escape),    // inert at the base Switch
        Ok(Key::Escape),    // still inert; fallback Ctrl-C quits
    ];
    assert!(matches!(run(keys, state).unwrap(), Outcome::Quit));
}

#[test]
fn text_modal_scrolls_and_dismisses() {
    // `man` (run from the palette) opens a scrollable text modal over it; the
    // arrows / j/k and PageUp/PageDown scroll it, Esc dismisses it (back to the
    // palette), a `PageUp` then exercises the palette's no-op catch-all, and Esc
    // closes the palette (fallback Ctrl-C quits).
    let mut keys = cmd("man");
    keys.push(Ok(Key::Enter)); // run `man` -> opens the text modal over the palette
    keys.push(Ok(Key::ArrowDown)); // scroll down a line
    keys.push(Ok(Key::Char('j')));
    keys.push(Ok(Key::ArrowUp)); // scroll up a line
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::PageDown)); // page down
    keys.push(Ok(Key::PageUp)); // page up
    keys.push(Ok(Key::Char('z'))); // ignored inside the modal
    keys.push(Ok(Key::Escape)); // dismiss the modal -> back on the palette
    keys.push(Ok(Key::PageUp)); // a no-op key in the palette (its catch-all)
    keys.push(Ok(Key::Escape)); // close the palette; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn preview_command_opens_reads_scrolls_and_dismisses_the_markdown_pane() {
    // `preview <file>` resolves and reads the file under the workspace root, opens
    // the right-pane preview, and then the arrows / j/k and PageUp/PageDown scroll
    // it while Esc dismisses it (back to the base Switch, where Ctrl-C quits).
    let dir = tempfile::tempdir().unwrap();
    let body = (0..40)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(dir.path().join("README.md"), format!("# Title\n{body}")).unwrap();

    let mut keys = cmd("preview README");
    keys.push(Ok(Key::Enter)); // run `preview` -> reads the file, opens the pane
    keys.push(Ok(Key::ArrowDown)); // scroll down a line
    keys.push(Ok(Key::Char('j')));
    keys.push(Ok(Key::ArrowUp)); // scroll up a line
    keys.push(Ok(Key::Char('k')));
    keys.push(Ok(Key::PageDown)); // page down
    keys.push(Ok(Key::PageUp)); // page up
    keys.push(Ok(Key::Char('z'))); // ignored inside the preview
    keys.push(Ok(Key::Escape)); // dismiss -> Switch
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
    assert!(matches!(
        run_at(keys, sample_state(), dir.path()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn preview_command_logs_a_failure_for_a_missing_file() {
    // A `preview` of a file that does not exist opens nothing and logs the error;
    // the screen keeps running and quits on the trailing Ctrl-C.
    let dir = tempfile::tempdir().unwrap();
    let mut keys = cmd("preview missing");
    keys.push(Ok(Key::Enter)); // run `preview` -> read fails, nothing opens
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch (no preview captured it)
    assert!(matches!(
        run_at(keys, sample_state(), dir.path()).unwrap(),
        Outcome::Quit
    ));
}

#[test]
fn overview_edits_completes_and_recalls_then_runs() {
    let mut keys = cmd("ma");
    keys.push(Ok(Key::Backspace));
    keys.push(Ok(Key::Tab)); // "m" -> "man"
    keys.push(Ok(Key::Enter)); // run -> `man` opens its text modal
    keys.push(Ok(Key::Escape)); // dismiss the modal -> Switch
    keys.push(Ok(Key::ArrowUp)); // recall the previous command
    keys.push(Ok(Key::ArrowDown)); // back to empty
    keys.push(Ok(Key::Home)); // ignored
    keys.push(Ok(Key::Escape)); // Esc inert at the base Switch; fallback Ctrl-C quits
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn overview_caret_keys_edit_within_the_line() {
    // Build "history" by typing out of order and moving the caret with the
    // editing keys, exercising ←/→/End/Del; the recorded command proves the
    // edits landed where the caret was.
    let mut keys = cmd("hstory"); // missing the 'i'
    for _ in 0..5 {
        keys.push(Ok(Key::ArrowLeft)); // caret to just after 'h'
    }
    keys.extend(typed("i")); // "history"
    keys.push(Ok(Key::End)); // jump to the end
    keys.extend(typed("X")); // "historyX"
    keys.push(Ok(Key::ArrowLeft)); // caret before the 'X'
    keys.push(Ok(Key::Del)); // delete it -> "history"
    keys.push(Ok(Key::ArrowRight)); // already at the end -> clamped no-op
    keys.push(Ok(Key::Enter)); // run `history` -> opens its text modal
    keys.push(Ok(Key::Escape)); // dismiss the modal
    keys.push(Ok(Key::Escape)); // Esc inert; fallback Ctrl-C quits

    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut recorded = Vec::new();
    let mut persist = |c: &str| recorded.push(c.to_string());
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
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
    assert_eq!(recorded, vec!["history"]);
}

#[test]
fn quit_command_exits_the_app() {
    let mut keys = cmd("quit");
    keys.push(Ok(Key::Enter));
    assert!(matches!(run(keys, sample_state()).unwrap(), Outcome::Quit));
}

#[test]
fn submitted_commands_are_handed_to_persist() {
    let mut keys = cmd("man");
    keys.push(Ok(Key::Enter));
    keys.push(Ok(Key::Escape));
    let term = Term::stdout();
    let mut reader = ScriptedReader::new(keys);
    let monitor = MonitorHandle::detached();
    let mut recorded = Vec::new();
    let mut persist = |c: &str| recorded.push(c.to_string());
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut remove: fn(&str, bool) -> SessionOutcome = noop_remove;
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
    assert_eq!(recorded, vec!["man"]);
}

#[test]
fn palette_refuses_session_scoped_commands() {
    // `terminal` / `agent` / `close` are session-scoped; the `:` palette is the
    // workspace surface, so dispatch refuses them (an error line, no action) and
    // the palette stays open. No pane is ever attached, however they are typed.
    let opened = RefCell::new(false);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() = true;
        Ok(PaneExit::Closed)
    };
    let mut create: fn(&str) -> SessionOutcome = noop_create;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let mut keys = cmd("terminal"); // open palette, type `terminal`
    keys.push(Ok(Key::Enter)); // refused -> palette stays open, input cleared
    keys.extend(typed("agent")); // type `agent` into the still-open palette
    keys.push(Ok(Key::Enter)); // refused
    keys.extend(typed("close")); // type `close`
    keys.push(Ok(Key::Enter)); // refused
    keys.push(Ok(Key::Escape)); // close the palette -> Switch
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
    assert!(!*opened.borrow(), "no session command should attach a pane");
}
