use super::*;

// --- caret editing -----------------------------------------------------

#[test]
fn arrows_move_the_caret_and_insert_mid_line() {
    let mut state = state();
    for c in "mn".chars() {
        state.push_char(c);
    }
    // Caret sits past the end after typing.
    assert_eq!(state.cursor(), 2);
    state.cursor_left();
    assert_eq!(state.cursor(), 1);
    // Insert between the two characters.
    state.push_char('a');
    assert_eq!(state.input(), "man");
    assert_eq!(state.cursor(), 2);
    // Right then past the end is clamped.
    state.cursor_right();
    state.cursor_right();
    assert_eq!(state.cursor(), 3);
    // Left at the start is clamped to 0.
    state.cursor_home();
    state.cursor_left();
    assert_eq!(state.cursor(), 0);
    state.cursor_end();
    assert_eq!(state.cursor(), 3);
}

#[test]
fn backspace_and_delete_act_around_the_caret() {
    let mut state = state();
    for c in "man".chars() {
        state.push_char(c);
    }
    state.cursor_home();
    // Backspace at the start is a no-op.
    state.backspace();
    assert_eq!(state.input(), "man");
    // Delete-forward removes the character at the caret.
    state.delete_forward();
    assert_eq!(state.input(), "an");
    assert_eq!(state.cursor(), 0);
    // Delete-forward at the end is a no-op.
    state.cursor_end();
    state.delete_forward();
    assert_eq!(state.input(), "an");
    // Backspace removes the character before the caret.
    state.backspace();
    assert_eq!(state.input(), "a");
    assert_eq!(state.cursor(), 1);
}

#[test]
fn caret_moves_by_whole_multibyte_characters() {
    let mut state = state();
    for c in "あい".chars() {
        state.push_char(c);
    }
    // Each Japanese character is three bytes; the caret tracks byte offsets but
    // moves a whole character at a time.
    assert_eq!(state.cursor(), 6);
    state.cursor_left();
    assert_eq!(state.cursor(), 3);
    state.push_char('x');
    assert_eq!(state.input(), "あxい");
    state.backspace();
    assert_eq!(state.input(), "あい");
    assert_eq!(state.cursor(), 3);
    state.delete_forward();
    assert_eq!(state.input(), "あ");
}

#[test]
fn recall_and_submit_place_the_caret_at_the_end() {
    let mut state = state();
    state.restore_history(vec!["session".to_string()]);
    state.recall_prev();
    assert_eq!(state.cursor(), state.input().len());
    state.recall_next();
    assert_eq!(state.cursor(), 0);
    state.push_char('m');
    state.submit();
    assert_eq!(state.cursor(), 0);
}

// --- 選択 (Overview) -----------------------------------------------------

#[test]
fn enter_overview_remembers_its_return_mode_and_moves_the_cursor() {
    let mut state = state(); // root, main, feature
    state.enter_overview(ReturnMode::Base);
    assert_eq!(state.mode(), Mode::Overview);
    assert_eq!(state.overview_return(), ReturnMode::Base);
    state.overview_move_down();
    assert_eq!(state.list().selected_index(), 1);
    state.overview_move_up();
    assert_eq!(state.list().selected_index(), 0);
    // Up from the root wraps to the bottom — the persistent "+ new session" row
    // (index 3, just past the two worktrees).
    state.overview_move_up();
    assert_eq!(state.list().selected_index(), 3);
    assert!(state.list().create_row_selected());
}

#[test]
fn overview_return_carries_each_origin() {
    let mut state = state();
    state.enter_overview(ReturnMode::Closeup);
    assert_eq!(state.overview_return(), ReturnMode::Closeup);
    state.enter_overview(ReturnMode::Attached);
    assert_eq!(state.overview_return(), ReturnMode::Attached);
}

#[test]
fn overview_inline_create_edits_then_confirms_a_fresh_name() {
    let mut state = state();
    state.enter_overview(ReturnMode::Base);
    assert!(!state.is_creating());
    state.overview_begin_create(Vec::new());
    assert!(state.is_creating());
    assert_eq!(state.create().unwrap().value(), "");
    {
        let input = state.create_mut().unwrap();
        for c in "  wip  ".chars() {
            input.push_char(c);
        }
        input.backspace(); // drop a trailing space
    }
    // A fresh, trimmed name is accepted and the input closes.
    assert_eq!(state.overview_confirm_create().as_deref(), Some("wip"));
    assert!(!state.is_creating());
}

#[test]
fn overview_inline_create_rejects_empty_and_duplicate_names() {
    let mut state = state();
    state.enter_overview(ReturnMode::Base);
    // The session "feature" would cut `usagi/feature`, which already exists, so
    // it is in the taken set (a bare `feature` would not collide).
    state.overview_begin_create(vec!["usagi/feature".to_string()]);
    // Whitespace only is empty after trimming: no live error (it does not nag),
    // but Enter rejects it.
    state.create_mut().unwrap().push_char(' ');
    assert!(state.create().unwrap().error().is_none());
    assert!(state.overview_confirm_create().is_none());
    assert!(state
        .create()
        .unwrap()
        .error()
        .unwrap()
        .contains("must not be empty"));
    // Typing a duplicate name flags it live, before Enter, and Enter rejects it.
    for c in "feature".chars() {
        state.create_mut().unwrap().push_char(c);
    }
    assert!(state.create().unwrap().error().unwrap().contains("feature"));
    assert!(state.overview_confirm_create().is_none());
    assert!(state.create().unwrap().error().unwrap().contains("feature"));
    assert!(state.is_creating());
}

#[test]
fn overview_inline_create_flags_a_branch_namespace_clash_live() {
    let mut state = state();
    state.enter_overview(ReturnMode::Base);
    // Branches nested under the session's namespaced branch `usagi/test/` make a
    // `test` session impossible.
    state.overview_begin_create(vec!["usagi/test/home-ui-e2e".to_string()]);
    for c in "test".chars() {
        state.create_mut().unwrap().push_char(c);
    }
    // The clash is shown live and blocks confirmation.
    let err = state.create().unwrap().error().unwrap().to_string();
    assert!(err.contains("conflicts with branch"), "{err}");
    assert!(err.contains("usagi/test/home-ui-e2e"), "{err}");
    assert!(state.overview_confirm_create().is_none());
    // Backspacing to "tes" (no longer a clash) clears the error.
    state.create_mut().unwrap().backspace();
    assert!(state.create().unwrap().error().is_none());
    // Typing a path separator is itself rejected (not a legal session name).
    state.create_mut().unwrap().push_char('/');
    assert!(state
        .create()
        .unwrap()
        .error()
        .unwrap()
        .contains("path separators"));
}

#[test]
fn overview_inline_create_can_be_cancelled() {
    let mut state = state();
    state.enter_overview(ReturnMode::Base);
    state.overview_begin_create(Vec::new());
    state.create_mut().unwrap().push_char('x');
    state.create_cancel();
    assert!(!state.is_creating());
}

#[test]
fn create_accessors_are_none_when_not_creating() {
    let mut state = state();
    // Nothing open: the accessors are empty and the lifecycle calls are safe.
    assert!(!state.is_creating());
    assert!(state.create().is_none());
    assert!(state.create_mut().is_none());
    assert!(state.overview_confirm_create().is_none());
    state.create_cancel();
    assert!(!state.is_creating());
}

#[test]
fn create_caret_moves_and_edits_mid_name() {
    let mut state = state();
    state.enter_overview(ReturnMode::Base);
    state.overview_begin_create(Vec::new());
    for c in "wip".chars() {
        state.create_mut().unwrap().push_char(c);
    }
    assert_eq!(state.create().unwrap().cursor(), 3);
    // Home, then insert at the front.
    state.create_mut().unwrap().move_home();
    assert_eq!(state.create().unwrap().cursor(), 0);
    state.create_mut().unwrap().push_char('x');
    assert_eq!(state.create().unwrap().value(), "xwip");
    assert_eq!(state.create().unwrap().cursor(), 1);
    // Del removes the character at the caret; Backspace the one before.
    state.create_mut().unwrap().delete_forward(); // removes 'w' → "xip"
    assert_eq!(state.create().unwrap().value(), "xip");
    state.create_mut().unwrap().move_right(); // between 'i' and 'p'
    state.create_mut().unwrap().backspace(); // removes 'i' → "xp"
    assert_eq!(state.create().unwrap().value(), "xp");
    // End parks the caret past the last character.
    state.create_mut().unwrap().move_end();
    assert_eq!(state.create().unwrap().cursor(), 2);
}

// --- 選択 (Overview) inline rename ---------------------------------------

#[test]
fn overview_inline_rename_prefills_edits_then_confirms_a_label() {
    let mut state = state(); // sessions: main, feature
    state.enter_overview(ReturnMode::Base);
    state.overview_move_down(); // cursor onto "main"
    assert!(state.overview_begin_rename());
    assert!(state.is_renaming());
    assert_eq!(state.rename().unwrap().target(), "main");
    // The input is pre-filled with the current label (the session name).
    assert_eq!(state.rename().unwrap().value(), "main");
    // Edit it to a custom label.
    {
        let input = state.rename_mut().unwrap();
        for _ in 0..4 {
            input.backspace();
        }
        for c in "  My main  ".chars() {
            input.push_char(c);
        }
    }
    // Confirm returns the target and the trimmed label, and closes the input.
    assert_eq!(
        state.overview_confirm_rename(),
        Some(("main".to_string(), "My main".to_string()))
    );
    assert!(!state.is_renaming());
}

#[test]
fn overview_begin_rename_is_a_noop_on_the_root_row_and_when_already_open() {
    let mut state = state();
    state.enter_overview(ReturnMode::Base);
    // Cursor on the root row: there is no session to rename.
    assert!(state.list().root_selected());
    assert!(!state.overview_begin_rename());
    assert!(!state.is_renaming());

    // On a session it opens, and a second begin while open is a no-op.
    state.overview_move_down();
    assert!(state.overview_begin_rename());
    assert!(!state.overview_begin_rename());

    // It also refuses to open while a create input is up.
    state.rename_cancel();
    state.overview_begin_create(Vec::new());
    assert!(!state.overview_begin_rename());
}

#[test]
fn rename_accessors_are_none_when_not_renaming() {
    let mut state = state();
    assert!(!state.is_renaming());
    assert!(state.rename().is_none());
    assert!(state.rename_mut().is_none());
    assert!(state.overview_confirm_rename().is_none());
}

#[test]
fn rename_can_be_cancelled() {
    let mut state = state();
    state.enter_overview(ReturnMode::Base);
    state.overview_move_down();
    state.overview_begin_rename();
    state.rename_mut().unwrap().push_char('x');
    state.rename_cancel();
    assert!(!state.is_renaming());
}

#[test]
fn rename_input_supports_caret_movement_and_forward_delete() {
    // The rename input has the same mid-string editing affordances as create: the
    // caret can move and a character can be deleted forward, not only at the end.
    let mut state = state();
    state.enter_overview(ReturnMode::Base);
    state.overview_move_down();
    assert!(state.overview_begin_rename());
    let rename = state.rename_mut().unwrap();
    // The input opens pre-filled with the session's current label; clear it so
    // the editing sequence starts from a known empty state.
    rename.move_end();
    while !rename.value().is_empty() {
        rename.backspace();
    }
    rename.push_char('a');
    rename.push_char('b');
    rename.push_char('d'); // "abd", caret at end
    rename.move_left(); // between 'b' and 'd'
    rename.push_char('c'); // "abcd"
    assert_eq!(rename.value(), "abcd");
    assert_eq!(rename.cursor(), 3);
    rename.move_home();
    rename.delete_forward(); // drop 'a' -> "bcd"
    assert_eq!(rename.value(), "bcd");
    rename.move_end();
    rename.backspace(); // drop 'd' -> "bc"
    rename.move_right(); // already at end: a no-op
    assert_eq!(rename.value(), "bc");
}

#[test]
fn restore_sessions_carries_the_display_name_onto_the_pane_label() {
    let mut state = state();
    let mut record = session_record("feature", 1);
    record.display_name = Some("Login flow".to_string());
    state.restore_sessions(vec![session_record("main", 1), record]);
    // Row 0 is the root; worktree index 0 = "main" (no override), 1 = "feature".
    assert_eq!(state.list().display_label(0), "main");
    assert_eq!(state.list().display_label(1), "Login flow");
    // The branch / identity is unchanged, so commands still key on it.
    assert_eq!(
        state.list().worktrees()[1].branch.as_deref(),
        Some("feature")
    );
}
