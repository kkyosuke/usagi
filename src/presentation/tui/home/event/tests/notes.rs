use super::*;

#[test]
fn switch_n_opens_the_note_editor_edits_the_buffer_and_saves() {
    // 切替, `n` on a session opens the editor; the editing keys build a
    // multi-line note, and Ctrl-S persists it through `set_note`.
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;

    let mut keys = vec![
        Ok(Key::Char(CTRL_O)),     // no-op at base Switch (cursor already on root)
        Ok(Key::ArrowDown),        // root -> alpha
        Ok(Key::Char('n')),        // open the note editor for alpha
        Ok(Key::Tab),              // ignored inside the editor
        Ok(Key::Char('\u{0001}')), // a control char (Ctrl-A): ignored
    ];
    keys.extend(typed("abc"));
    keys.push(Ok(Key::ArrowLeft));
    keys.push(Ok(Key::ArrowRight));
    keys.push(Ok(Key::Home));
    keys.push(Ok(Key::End));
    keys.push(Ok(Key::Backspace)); // "abc" -> "ab"
    keys.push(Ok(Key::Del)); // at end of buffer: no-op
    keys.push(Ok(Key::Enter)); // "ab" -> "ab\n"
    keys.push(Ok(Key::Char('z'))); // "ab\nz"
    keys.push(Ok(Key::ArrowUp));
    keys.push(Ok(Key::ArrowDown));
    keys.push(Ok(Key::Char(CTRL_S))); // save
    keys.push(Ok(Key::Escape)); // inert at the base Switch
    keys.push(Ok(Key::CtrlC)); // quit

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha", "beta"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *recorded.borrow(),
        vec![("alpha".to_string(), "ab\nz".to_string())]
    );
}

#[test]
fn switch_n_on_the_root_row_edits_and_saves_the_workspace_root_note() {
    // 切替, `n` on the `⌂ root` row opens the editor targeting the workspace root
    // (`root`); Ctrl-S persists it through `set_note` under that name.
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;

    // The cursor starts on the root row, so `n` edits the root note straight away.
    let mut keys = vec![Ok(Key::Char('n'))];
    keys.extend(typed("hi"));
    keys.push(Ok(Key::Char(CTRL_S))); // save
    keys.push(Ok(Key::CtrlC)); // quit

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *recorded.borrow(),
        vec![("root".to_string(), "hi".to_string())]
    );
}

#[test]
fn shift_arrows_select_text_and_delete_removes_the_selection() {
    // In the note editor, `Shift`+a cursor key extends a selection and `Del`
    // removes the whole span. Every selection direction is exercised, then the
    // surviving text is saved through `set_note`.
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;

    let mut keys = vec![
        Ok(Key::Char(CTRL_O)), // no-op at base Switch (cursor already on root)
        Ok(Key::ArrowDown),    // root -> alpha
        Ok(Key::Char('n')),    // open the note editor for alpha
    ];
    keys.extend(typed("hello world"));
    keys.push(Ok(Key::Home)); // caret to the line start (clears any selection)
    keys.push(shift_arrow('B')); // Shift+Down: single line, an empty extend
    keys.push(shift_arrow('A')); // Shift+Up: likewise
    keys.push(shift_arrow('F')); // Shift+End: select the whole line
    keys.push(shift_arrow('H')); // Shift+Home: collapse back to the start
    keys.push(shift_arrow('C')); // Shift+Right x5: select "hello"
    keys.push(shift_arrow('C'));
    keys.push(shift_arrow('C'));
    keys.push(shift_arrow('C'));
    keys.push(shift_arrow('C'));
    keys.push(shift_arrow('D')); // Shift+Left: shrink to "hell"
    keys.push(Ok(Key::Del)); // delete the selection -> "o world"
    keys.push(Ok(Key::Char(CTRL_S))); // save
    keys.push(Ok(Key::Escape)); // inert at the base Switch
    keys.push(Ok(Key::CtrlC)); // quit

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha", "beta"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *recorded.borrow(),
        vec![("alpha".to_string(), "o world".to_string())]
    );
}

#[test]
fn switch_ctrl_e_opens_the_note_editor_like_n() {
    // 切替, `Ctrl-E` (matching 在席 / 没入) opens the highlighted session's note
    // editor just like `n`; Ctrl-S persists it through `set_note`.
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;

    let mut keys = vec![
        Ok(Key::Char(CTRL_O)), // no-op at base Switch (cursor already on root)
        Ok(Key::ArrowDown),    // root -> alpha
        Ok(Key::Char(CTRL_E)), // open the note editor for alpha
    ];
    keys.extend(typed("hi"));
    keys.push(Ok(Key::Char(CTRL_S))); // save
    keys.push(Ok(Key::Escape)); // inert at the base Switch
    keys.push(Ok(Key::CtrlC)); // quit

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha", "beta"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *recorded.borrow(),
        vec![("alpha".to_string(), "hi".to_string())]
    );
}

#[test]
fn switch_end_key_opens_the_note_editor_like_ctrl_e() {
    // `console` decodes Ctrl-E as `Key::End`, so on a real terminal the chord
    // arrives as `End`; in 切替 list navigation (no caret) it opens the note just
    // like `Ctrl-E` / `n`.
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;

    let mut keys = vec![
        Ok(Key::Char(CTRL_O)), // no-op at base Switch (cursor already on root)
        Ok(Key::ArrowDown),    // root -> alpha
        Ok(Key::End),          // Ctrl-E as console delivers it: open the note
    ];
    keys.extend(typed("hi"));
    keys.push(Ok(Key::Char(CTRL_S))); // save
    keys.push(Ok(Key::Escape)); // inert at the base Switch
    keys.push(Ok(Key::CtrlC)); // quit

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha", "beta"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *recorded.borrow(),
        vec![("alpha".to_string(), "hi".to_string())]
    );
}

#[test]
fn switch_n_note_editor_cancel_discards_the_edit() {
    // Esc closes the editor without persisting anything.
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };
    let mut open: fn(&mut HomeState, &Path, bool, bool) -> Result<PaneExit> = noop_open;
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;

    let mut keys = vec![
        Ok(Key::Char(CTRL_O)), // no-op at base Switch
        Ok(Key::ArrowDown),    // root -> alpha
        Ok(Key::Char('n')),    // open the editor
    ];
    keys.extend(typed("draft"));
    keys.push(Ok(Key::Escape)); // cancel the editor (no save)
    keys.push(Ok(Key::Escape)); // inert at the base Switch
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert!(recorded.borrow().is_empty(), "cancel must not save");
}

#[test]
fn attached_ctrl_e_opens_the_note_editor_then_re_attaches_on_save() {
    // Attaching a live session, then `Ctrl-E` (reported as PaneExit::OpenNote)
    // opens the note editor over the pane; saving persists the note and
    // re-attaches (open_terminal is driven a second time).
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        let mut n = opened.borrow_mut();
        *n += 1;
        // First attach yields to the note editor; the re-attach then closes.
        if *n == 1 {
            Ok(PaneExit::OpenNote)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = cmd("session switch alpha");
    keys.push(Ok(Key::Enter)); // focus + attach alpha -> open_terminal #1 -> OpenNote
    keys.extend(typed("hi")); // edit the note in the editor
    keys.push(Ok(Key::Char(CTRL_S))); // save -> re-attach -> open_terminal #2 -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *opened.borrow(),
        2,
        "the pane is re-attached after the editor"
    );
    assert_eq!(
        *recorded.borrow(),
        vec![("alpha".to_string(), "hi".to_string())]
    );
}

#[test]
fn attached_ctrl_e_re_attaches_on_cancel_without_saving() {
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        let mut n = opened.borrow_mut();
        *n += 1;
        if *n == 1 {
            Ok(PaneExit::OpenNote)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = cmd("session switch alpha");
    keys.push(Ok(Key::Enter)); // attach -> OpenNote
    keys.extend(typed("scratch"));
    keys.push(Ok(Key::Escape)); // cancel -> re-attach -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(*opened.borrow(), 2, "cancel still re-attaches the pane");
    assert!(recorded.borrow().is_empty(), "cancel must not save");
}

#[test]
fn attached_ctrl_e_on_the_root_row_opens_the_editor_and_saves_the_workspace_root_note() {
    // The root row carries its own note: Ctrl-E in 没入 (reported as OpenNote)
    // opens the editor over the (detached) pane, and Ctrl-S persists it under
    // `root` and re-attaches — exactly like a session.
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        let mut n = opened.borrow_mut();
        *n += 1;
        if *n == 1 {
            Ok(PaneExit::OpenNote)
        } else {
            Ok(PaneExit::Closed)
        }
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = live_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = cmd("session switch root");
    keys.push(Ok(Key::Enter)); // focus + attach root -> OpenNote -> editor
    keys.extend(typed("ws"));
    keys.push(Ok(Key::Char(CTRL_S))); // save -> re-attach -> Closed -> Focus
    keys.push(Ok(Key::Escape)); // Focus -> Switch
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *opened.borrow(),
        2,
        "the root pane is re-attached after saving"
    );
    assert_eq!(
        *recorded.borrow(),
        vec![("root".to_string(), "ws".to_string())]
    );
}

#[test]
fn focus_ctrl_e_opens_the_note_editor_and_saves_staying_in_focus() {
    // In 在席 (Focus), Ctrl-E opens the focused session's note editor; saving
    // persists the note and returns to 在席 (no pane to re-attach). We prove the
    // landing mode by pressing `t` afterwards — a 在席 menu shortcut that launches
    // a terminal — so the pane callback runs only if we are still in Focus.
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = cmd("session switch alpha");
    keys.push(Ok(Key::Enter)); // focus alpha (idle -> 在席 menu)
    keys.push(Ok(Key::Char(CTRL_E))); // open the note editor (reattach = false)
    keys.extend(typed("todo"));
    keys.push(Ok(Key::Char(CTRL_S))); // save -> back to 在席
    keys.push(Ok(Key::Char('t'))); // 在席 menu: launch terminal (proves we are in Focus)
    keys.push(Ok(Key::Escape)); // Focus -> Switch
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *recorded.borrow(),
        vec![("alpha".to_string(), "todo".to_string())]
    );
    assert_eq!(
        *opened.borrow(),
        1,
        "`t` after save launched a terminal, so we stayed in 在席"
    );
}

#[test]
fn focus_end_key_opens_the_note_editor_on_the_menu_surface() {
    // `console` decodes Ctrl-E as `Key::End`, so on a real terminal the chord
    // arrives as `End`. On 在席's menu surface (the default — no caret) it opens
    // the note just like the scripted `Ctrl-E`. (The typed prompt keeps `End` as
    // end-of-line; that path is covered by `focus_prompt_edits_*`.)
    let opened = RefCell::new(0);
    let mut open = |_h: &mut HomeState, _d: &Path, _a: bool, _n: bool| {
        *opened.borrow_mut() += 1;
        Ok(PaneExit::Closed)
    };
    let mut preview: fn(&Path, Sidebar) -> Option<TerminalView> = noop_preview;
    let recorded = RefCell::new(Vec::<(String, String)>::new());
    let mut set_note = |name: &str, text: &str| {
        recorded
            .borrow_mut()
            .push((name.to_string(), text.to_string()));
        noop_set_note(name, text)
    };

    let mut keys = cmd("session switch alpha");
    keys.push(Ok(Key::Enter)); // focus alpha (idle -> 在席 menu)
    keys.push(Ok(Key::End)); // Ctrl-E as console delivers it: open the note
    keys.extend(typed("todo"));
    keys.push(Ok(Key::Char(CTRL_S))); // save -> back to 在席
    keys.push(Ok(Key::Char('t'))); // 在席 menu: launch terminal (proves we are in Focus)
    keys.push(Ok(Key::Escape)); // Focus -> Switch
    keys.push(Ok(Key::CtrlC));

    let outcome = run_notes(
        keys,
        state_with_sessions(&["alpha"]),
        &mut open,
        &mut preview,
        &mut set_note,
    )
    .unwrap();
    assert!(matches!(outcome, Outcome::Quit));
    assert_eq!(
        *recorded.borrow(),
        vec![("alpha".to_string(), "todo".to_string())]
    );
    assert_eq!(
        *opened.borrow(),
        1,
        "`t` after save launched a terminal, so we stayed in 在席"
    );
}
