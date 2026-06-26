use super::*;

#[test]
fn switch_begin_note_opens_the_editor_prefilled_with_the_sessions_note() {
    let mut state = state_on_alpha();
    assert!(state.switch_begin_note());
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), "alpha");
    // Pre-filled with the recorded note, caret parked at its end.
    assert_eq!(editor.area().text(), "existing");
    assert!(!editor.reattach());
    assert!(!state.note_editor_reattaches());

    // A second begin is a no-op while one is already open.
    assert!(!state.switch_begin_note());
}

#[test]
fn switch_begin_note_is_a_noop_on_the_root_row() {
    // The cursor starts on the root row, which is the workspace, not a session.
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    assert!(!state.switch_begin_note());
    assert!(state.note_editor().is_none());
    // The mutable accessor is likewise empty when no editor is open.
    assert!(state.note_editor_mut().is_none());
}

#[test]
fn open_focused_note_targets_the_active_session_and_carries_reattach() {
    let mut state = state_on_alpha();
    state.enter_focus(state.list().selected_index()); // 在席 on alpha
                                                      // 没入's `Ctrl-E` opens with reattach = true.
    assert!(state.open_focused_note(true));
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), "alpha");
    assert!(editor.reattach());
    assert!(state.note_editor_reattaches());
    // Already open: a second open is refused.
    assert!(!state.open_focused_note(true));

    // 在席's `Ctrl-E` opens with reattach = false (close returns to the action
    // surface, no pane to re-attach).
    state.note_editor_cancel();
    assert!(state.open_focused_note(false));
    assert!(!state.note_editor_reattaches());
}

#[test]
fn open_focused_note_is_a_noop_on_the_root_row() {
    // The root row is focused by default; it has no note to edit.
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.enter_focus(0);
    assert!(!state.open_focused_note(false));
    assert!(state.note_editor().is_none());
}

#[test]
fn note_editor_edits_confirm_and_cancel() {
    let mut state = state_on_alpha();
    // A session with no note opens an empty editor.
    let mut beta = session_record("beta", 1);
    beta.note = None;
    state.restore_sessions(vec![session_record("alpha", 1), beta]);
    state.switch_move_down();
    state.switch_move_down(); // alpha -> beta
    assert!(state.switch_begin_note());
    let area = state.note_editor_mut().unwrap().area_mut();
    assert!(area.is_empty());
    area.insert('h');
    area.insert('i');
    // Confirm returns the target, the typed text, and reattach=false (切替).
    let (target, text, reattach) = state.confirm_note_editor().unwrap();
    assert_eq!(target, "beta");
    assert_eq!(text, "hi");
    assert!(!reattach);
    assert!(state.note_editor().is_none());
    // Confirm / cancel with nothing open are no-ops.
    assert!(state.confirm_note_editor().is_none());

    // Cancel discards an open editor.
    state.switch_begin_note();
    assert!(state.note_editor().is_some());
    state.note_editor_cancel();
    assert!(state.note_editor().is_none());
    assert!(!state.note_editor_reattaches());
}

#[test]
fn selected_session_note_reads_the_cursor_rows_note() {
    // `state_on_alpha` records alpha with the note "existing" and parks the
    // cursor on it.
    let state = state_on_alpha();
    assert_eq!(state.selected_session_note(), Some("existing"));
}

#[test]
fn selected_session_note_is_none_on_root_and_for_a_noteless_session() {
    let mut state = state();
    // `session_record` records no note.
    state.restore_sessions(vec![session_record("alpha", 1)]);
    // The cursor starts on the root row (not a session).
    assert_eq!(state.selected_session_note(), None);
    // Moving onto a session with no note still reports `None`.
    state.switch_move_down();
    assert_eq!(state.selected_session_note(), None);
}
