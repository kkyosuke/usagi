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
