use super::*;

#[test]
fn set_now_records_the_frame_render_time() {
    let mut state = state();
    let pinned = chrono::DateTime::parse_from_rfc3339("2026-06-27T09:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    state.set_now(pinned);
    assert_eq!(state.now(), pinned);
}

#[test]
fn session_row_takes_last_active_as_its_freshness() {
    let mut state = state();
    let created = chrono::DateTime::parse_from_rfc3339("2026-06-20T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let git_synced = chrono::DateTime::parse_from_rfc3339("2026-06-26T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let touched = chrono::DateTime::parse_from_rfc3339("2026-06-25T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut session = session_record("multi", 0);
    session.created_at = created;
    // The worktree git-sync time is reset for every session on each workspace
    // sync, so the collapsed row's freshness tracks the session's `last_active`
    // (when it was last touched), not the worktrees' `updated_at`.
    let mut wt = worktree("multi");
    wt.updated_at = git_synced;
    session.worktrees = vec![wt];
    session.last_active = Some(touched);
    state.restore_sessions(vec![session]);
    assert_eq!(state.list().worktrees()[0].updated_at, touched);
}

#[test]
fn session_row_with_no_worktrees_falls_back_to_the_created_at() {
    let mut state = state();
    let created = chrono::DateTime::parse_from_rfc3339("2026-06-01T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut session = session_record("empty", 0);
    session.worktrees = vec![];
    session.created_at = created;
    state.restore_sessions(vec![session]);
    assert_eq!(state.list().worktrees()[0].updated_at, created);
}

#[test]
fn session_row_sums_ahead_behind_across_the_sessions_worktrees() {
    use crate::domain::workspace_state::AheadBehind;
    let mut state = state();
    let mut session = session_record("multi", 0);
    let mut a = worktree("multi");
    a.ahead_behind = Some(AheadBehind {
        ahead: 2,
        behind: 1,
    });
    let mut b = worktree("multi");
    b.ahead_behind = Some(AheadBehind {
        ahead: 3,
        behind: 0,
    });
    session.worktrees = vec![a, b];
    state.restore_sessions(vec![session]);
    assert_eq!(
        state.list().worktrees()[0].ahead_behind,
        Some(AheadBehind {
            ahead: 5,
            behind: 1
        })
    );
}

#[test]
fn rebuilding_the_list_marks_rows_whose_session_carries_a_note() {
    let mut state = state();
    let mut alpha = session_record("alpha", 1);
    alpha.note = Some("a memo".to_string());
    let beta = session_record("beta", 1); // no note
    state.restore_sessions(vec![alpha, beta]);
    // Row 0 maps to alpha (note), row 1 to beta (none); the pane shows the memo
    // marker only for the session that has one.
    assert!(state.list().has_note(0));
    assert!(!state.list().has_note(1));
}

#[test]
fn overview_begin_note_opens_the_editor_prefilled_with_the_sessions_note() {
    let mut state = state_on_alpha();
    assert!(state.overview_begin_note());
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), "alpha");
    // Pre-filled with the recorded note, caret parked at its end.
    assert_eq!(editor.area().text(), "existing");
    assert!(!editor.reattach());
    assert!(!state.note_editor_reattaches());

    // A second begin is a no-op while one is already open.
    assert!(!state.overview_begin_note());
}

#[test]
fn overview_begin_note_on_the_root_row_edits_the_workspace_root_note() {
    // The cursor starts on the root row: editing its note targets the workspace
    // root (`ROOT_NAME`), pre-filled with the recorded root note.
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.restore_root_note(Some("root memo".to_string()));
    // Before any editor is open the mutable accessor is empty.
    assert!(state.note_editor_mut().is_none());
    assert!(state.overview_begin_note());
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), ROOT_NAME);
    assert_eq!(editor.area().text(), "root memo");
}

#[test]
fn overview_begin_note_on_the_root_row_opens_empty_without_a_root_note() {
    // No root note recorded: the editor opens blank but still targets the root.
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    assert!(state.overview_begin_note());
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), ROOT_NAME);
    assert!(editor.area().is_empty());
}

#[test]
fn open_focused_note_targets_the_active_session_and_carries_reattach() {
    let mut state = state_on_alpha();
    state.enter_closeup(state.list().selected_index()); // 集中 on alpha
                                                        // 没入's `Ctrl-E` opens with reattach = true.
    assert!(state.open_focused_note(true));
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), "alpha");
    assert!(editor.reattach());
    assert!(state.note_editor_reattaches());
    // Already open: a second open is refused.
    assert!(!state.open_focused_note(true));

    // 集中's `Ctrl-E` opens with reattach = false (close returns to the action
    // surface, no pane to re-attach).
    state.note_editor_cancel();
    assert!(state.open_focused_note(false));
    assert!(!state.note_editor_reattaches());
}

#[test]
fn open_focused_note_on_the_root_row_edits_the_workspace_root_note() {
    // Focusing the root row and opening its note targets the workspace root,
    // pre-filled with the recorded root note.
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.restore_root_note(Some("root memo".to_string()));
    state.enter_closeup(0);
    assert!(state.open_focused_note(false));
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), ROOT_NAME);
    assert_eq!(editor.area().text(), "root memo");
}

#[test]
fn note_editor_edits_confirm_and_cancel() {
    let mut state = state_on_alpha();
    // A session with no note opens an empty editor.
    let mut beta = session_record("beta", 1);
    beta.note = None;
    state.restore_sessions(vec![session_record("alpha", 1), beta]);
    state.overview_move_down();
    state.overview_move_down(); // alpha -> beta
    assert!(state.overview_begin_note());
    let area = state.note_editor_mut().unwrap().area_mut();
    assert!(area.is_empty());
    area.insert('h');
    area.insert('i');
    // Confirm returns the target, the typed text, no todos (untouched), and
    // reattach=false (選択).
    let (target, text, todos, reattach) = state.confirm_note_editor().unwrap();
    assert_eq!(target, "beta");
    assert_eq!(text, "hi");
    assert!(todos.is_none());
    assert!(!reattach);
    assert!(state.note_editor().is_none());
    // Confirm / cancel with nothing open are no-ops.
    assert!(state.confirm_note_editor().is_none());

    // Cancel discards an open editor.
    state.overview_begin_note();
    assert!(state.note_editor().is_some());
    state.note_editor_cancel();
    assert!(state.note_editor().is_none());
    assert!(!state.note_editor_reattaches());
}

#[test]
fn note_editor_opens_focused_on_note_with_the_sessions_todos_and_decisions() {
    let mut state = state();
    let mut alpha = session_record("alpha", 1);
    alpha.note = Some("memo".to_string());
    alpha.todos = vec![SessionTodo::new("write tests"), {
        let mut td = SessionTodo::new("ship");
        td.done = true;
        td
    }];
    alpha.decisions = vec![SessionDecision {
        at: Utc::now(),
        text: "chose approach A".to_string(),
    }];
    state.restore_sessions(vec![alpha]);
    state.overview_move_down(); // root -> alpha

    assert!(state.overview_begin_note());
    let editor = state.note_editor().expect("editor open");
    // Opens focused on the note pane, pre-filled, carrying read-only snapshots.
    assert_eq!(editor.focus(), NotePane::Note);
    assert_eq!(editor.area().text(), "memo");
    assert_eq!(editor.todos().len(), 2);
    assert!(editor.todos()[1].done);
    assert_eq!(editor.decisions().len(), 1);
    assert_eq!(editor.decisions()[0].text, "chose approach A");
}

#[test]
fn note_editor_cycle_focus_wraps_forward_and_backward() {
    let state_labels: Vec<&str> = NotePane::all().iter().map(|t| t.label()).collect();
    assert_eq!(state_labels, ["note", "todos", "decisions"]);

    let mut state = state_on_alpha();
    // No editor open → cycling is a no-op that reports it did nothing.
    assert!(!state.note_editor_cycle_focus(true));

    assert!(state.overview_begin_note());
    assert_eq!(state.note_editor().unwrap().focus(), NotePane::Note);
    // Forward: note -> todos -> decisions -> note (wrap).
    assert!(state.note_editor_cycle_focus(true));
    assert_eq!(state.note_editor().unwrap().focus(), NotePane::Todos);
    state.note_editor_cycle_focus(true);
    assert_eq!(state.note_editor().unwrap().focus(), NotePane::Decisions);
    state.note_editor_cycle_focus(true);
    assert_eq!(state.note_editor().unwrap().focus(), NotePane::Note);
    // Backward from note wraps to decisions.
    state.note_editor_cycle_focus(false);
    assert_eq!(state.note_editor().unwrap().focus(), NotePane::Decisions);
}

fn state_on_alpha_with_todos() -> HomeState {
    let mut state = state();
    let mut alpha = session_record("alpha", 1);
    alpha.todos = vec![SessionTodo::new("first"), SessionTodo::new("second")];
    state.restore_sessions(vec![alpha]);
    state.overview_move_down(); // root -> alpha
    state.overview_begin_note();
    state.note_editor_cycle_focus(true); // note -> todos
    state
}

#[test]
fn todos_pane_move_toggle_and_delete() {
    let mut state = state_on_alpha_with_todos();
    assert!(state.note_editor_todos_list_active());
    assert!(!state.note_editor_todo_input_active());
    assert_eq!(state.note_editor().unwrap().selected_todo(), 0);

    // Move down clamps at the last row; up saturates at 0.
    state.note_editor_move_todo(true);
    assert_eq!(state.note_editor().unwrap().selected_todo(), 1);
    state.note_editor_move_todo(true); // clamp at last
    assert_eq!(state.note_editor().unwrap().selected_todo(), 1);
    state.note_editor_move_todo(false);
    state.note_editor_move_todo(false); // saturate at 0
    assert_eq!(state.note_editor().unwrap().selected_todo(), 0);

    // Toggle the selected todo's done state.
    state.note_editor_toggle_todo();
    assert!(state.note_editor().unwrap().todos()[0].done);

    // Delete the selected todo; selection clamps to what remains.
    state.note_editor_move_todo(true); // select "second"
    state.note_editor_remove_todo();
    assert_eq!(state.note_editor().unwrap().todos().len(), 1);
    assert_eq!(state.note_editor().unwrap().selected_todo(), 0);

    // Confirm reports the changed todos.
    let (_, _, todos, _) = state.confirm_note_editor().unwrap();
    let todos = todos.expect("todos were edited");
    assert_eq!(todos.len(), 1);
    assert!(todos[0].done);
    assert_eq!(todos[0].text, "first");
}

#[test]
fn todos_pane_add_via_inline_input() {
    let mut state = state_on_alpha_with_todos();
    state.note_editor_begin_add_todo();
    assert!(state.note_editor_todo_input_active());
    // The list keys are inert while the input is open.
    assert!(!state.note_editor_todos_list_active());
    let input = state.note_editor().unwrap().todo_input().unwrap();
    assert!(!input.is_editing());

    for c in "third".chars() {
        state.note_editor_todo_input_key(&console::Key::Char(c));
    }
    state.note_editor_commit_todo_input();
    assert!(!state.note_editor_todo_input_active());
    let todos = state.note_editor().unwrap().todos();
    assert_eq!(todos.len(), 3);
    assert_eq!(todos[2].text, "third");
    // The new row becomes selected.
    assert_eq!(state.note_editor().unwrap().selected_todo(), 2);
}

#[test]
fn todos_pane_edit_and_cancel_inline_input() {
    let mut state = state_on_alpha_with_todos();
    // Edit the first todo, prefilled with its text.
    state.note_editor_begin_edit_todo();
    assert!(state
        .note_editor()
        .unwrap()
        .todo_input()
        .unwrap()
        .is_editing());
    // Replace the text: clear then type.
    for _ in 0.."first".len() {
        state.note_editor_todo_input_key(&console::Key::Backspace);
    }
    for c in "edited".chars() {
        state.note_editor_todo_input_key(&console::Key::Char(c));
    }
    state.note_editor_commit_todo_input();
    assert_eq!(state.note_editor().unwrap().todos()[0].text, "edited");

    // Begin an add, then cancel it — the list is unchanged and the input closes.
    state.note_editor_begin_add_todo();
    state.note_editor_todo_input_key(&console::Key::Char('x'));
    state.note_editor_cancel_todo_input();
    assert!(!state.note_editor_todo_input_active());
    assert_eq!(state.note_editor().unwrap().todos().len(), 2);
}

#[test]
fn todo_list_ops_are_inert_while_the_inline_input_is_open() {
    let mut state = state_on_alpha_with_todos();
    state.note_editor_begin_add_todo();
    // While the input captures the keyboard, the list keys do nothing.
    state.note_editor_toggle_todo();
    state.note_editor_remove_todo();
    state.note_editor_move_todo(true);
    state.note_editor_begin_edit_todo();
    assert!(state.note_editor_todo_input_active());
    assert_eq!(state.note_editor().unwrap().todos().len(), 2);
    assert!(!state.note_editor().unwrap().todos()[0].done);
    assert_eq!(state.note_editor().unwrap().selected_todo(), 0);

    // Committing with no input open is a no-op.
    state.note_editor_cancel_todo_input();
    state.note_editor_commit_todo_input();
    assert_eq!(state.note_editor().unwrap().todos().len(), 2);
}

#[test]
fn todos_editing_ops_are_no_ops_on_an_empty_list_and_without_an_editor() {
    // Empty list: move / toggle / delete / edit do nothing; add still works.
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.overview_move_down();
    state.overview_begin_note();
    state.note_editor_cycle_focus(true); // todos
    state.note_editor_move_todo(true);
    state.note_editor_toggle_todo();
    state.note_editor_remove_todo();
    state.note_editor_begin_edit_todo(); // no-op: nothing to edit
    assert!(!state.note_editor_todo_input_active());
    assert!(state.note_editor().unwrap().todos().is_empty());
    // An empty commit just closes the input without adding a blank row.
    state.note_editor_begin_add_todo();
    state.note_editor_commit_todo_input();
    assert!(state.note_editor().unwrap().todos().is_empty());
    // Untouched todos → confirm reports None.
    let (_, _, todos, _) = state.confirm_note_editor().unwrap();
    assert!(todos.is_none());

    // With no editor open, the wrappers are inert no-ops.
    state.note_editor_move_todo(true);
    state.note_editor_toggle_todo();
    state.note_editor_remove_todo();
    state.note_editor_begin_add_todo();
    state.note_editor_begin_edit_todo();
    state.note_editor_todo_input_key(&console::Key::Char('x'));
    state.note_editor_commit_todo_input();
    state.note_editor_cancel_todo_input();
    assert!(!state.note_editor_todos_list_active());
    assert!(!state.note_editor_todo_input_active());
}

#[test]
fn note_editor_on_the_root_row_has_empty_todos_and_decisions() {
    // The root scratchpad's todos/decisions are not mirrored into the sidebar
    // state, so the editor opens them empty (the note still edits `root_note`).
    let mut state = state();
    state.restore_root_note(Some("root memo".to_string()));
    assert!(state.overview_begin_note()); // cursor starts on the root row
    let editor = state.note_editor().expect("editor open");
    assert_eq!(editor.target(), ROOT_NAME);
    assert_eq!(editor.area().text(), "root memo");
    assert!(editor.todos().is_empty());
    assert!(editor.decisions().is_empty());
}

#[test]
fn selected_session_note_reads_the_cursor_rows_note() {
    // `state_on_alpha` records alpha with the note "existing" and parks the
    // cursor on it.
    let state = state_on_alpha();
    assert_eq!(state.selected_session_note(), Some("existing"));
}

#[test]
fn selected_session_note_is_none_without_a_root_note_or_for_a_noteless_session() {
    let mut state = state();
    // `session_record` records no note.
    state.restore_sessions(vec![session_record("alpha", 1)]);
    // The cursor starts on the root row, which carries no note here.
    assert_eq!(state.selected_session_note(), None);
    // Moving onto a session with no note still reports `None`.
    state.overview_move_down();
    assert_eq!(state.selected_session_note(), None);
}

#[test]
fn selected_session_note_reads_the_root_note_on_the_root_row() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    state.restore_root_note(Some("root memo".to_string()));
    // The cursor starts on the root row, so its note is the workspace root note.
    assert_eq!(state.selected_session_note(), Some("root memo"));
}

#[test]
fn restore_root_note_marks_the_root_row_and_exposes_the_note() {
    let mut state = state();
    // No root note: the root row carries no marker.
    assert!(!state.list().root_has_note());
    assert_eq!(state.root_note(), None);
    // Restoring one marks the root row and exposes the note.
    state.restore_root_note(Some("memo".to_string()));
    assert!(state.list().root_has_note());
    assert_eq!(state.root_note(), Some("memo"));
    // Clearing it drops the marker again.
    state.restore_root_note(None);
    assert!(!state.list().root_has_note());
    assert_eq!(state.root_note(), None);
}

#[test]
fn apply_session_outcome_updates_the_root_note_and_its_marker() {
    let mut state = state();
    state.restore_sessions(vec![session_record("alpha", 1)]);
    // A root-note save reports the stored note (and reloads the sessions).
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("Saved note for \"root\" 📝"),
        sessions: Some(vec![session_record("alpha", 1)]),
        select: None,
        root_note: Some(Some("saved".to_string())),
    });
    assert_eq!(state.root_note(), Some("saved"));
    assert!(state.list().root_has_note());

    // Clearing it (inner `None`) drops the note and the marker.
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("Cleared note for \"root\" 📝"),
        sessions: Some(vec![session_record("alpha", 1)]),
        select: None,
        root_note: Some(None),
    });
    assert_eq!(state.root_note(), None);
    assert!(!state.list().root_has_note());

    // An outcome that does not touch the root note (`None`) leaves it as is.
    state.restore_root_note(Some("kept".to_string()));
    state.apply_session_outcome(SessionOutcome {
        line: LogLine::output("renamed"),
        sessions: None,
        select: None,
        root_note: None,
    });
    assert_eq!(state.root_note(), Some("kept"));
}
