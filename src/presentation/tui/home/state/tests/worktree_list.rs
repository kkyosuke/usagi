use super::*;

#[test]
fn new_list_starts_on_the_root_row() {
    let list = sample();
    assert_eq!(list.workspace_name(), "usagi");
    // The cursor starts on the root row, which belongs to no session.
    assert_eq!(list.selected_index(), 0);
    assert!(list.root_selected());
    assert!(list.selected().is_none());
    assert_eq!(list.worktrees().len(), 3);
    assert!(!list.is_empty());
}

#[test]
fn empty_list_still_has_the_root_row() {
    let list = WorktreeList::new("usagi", Vec::new());
    assert!(list.is_empty());
    assert!(list.root_selected());
    // The root row has no worktree behind it.
    assert!(list.selected().is_none());
}

#[test]
fn display_label_uses_the_override_then_falls_back_to_the_branch() {
    // A labels vec shorter than the worktrees is padded with `None`; a longer
    // one is truncated to match.
    let list = WorktreeList::with_labels(
        "usagi",
        vec![worktree("main"), worktree("feature"), worktree("fix")],
        vec![Some("Main".to_string()), None],
    );
    assert_eq!(list.display_label(0), "Main"); // override
    assert_eq!(list.display_label(1), "feature"); // explicit None → branch
    assert_eq!(list.display_label(2), "fix"); // padded None → branch
                                              // An out-of-range index has neither a label nor a worktree.
    assert_eq!(list.display_label(9), "");
}

#[test]
fn move_down_advances_past_the_root_row_and_wraps() {
    let mut list = sample(); // root, main, feature, fix
    list.move_down();
    assert_eq!(list.selected_index(), 1);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("main"));
    list.move_down();
    list.move_down();
    assert_eq!(list.selected_index(), 3);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
    // Wraps from the last worktree back to the root row.
    list.move_down();
    assert_eq!(list.selected_index(), 0);
    assert!(list.root_selected());
}

#[test]
fn move_up_wraps_from_the_root_row_to_the_bottom() {
    let mut list = sample(); // root, main, feature, fix
    list.move_up();
    assert_eq!(list.selected_index(), 3);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
    list.move_up();
    assert_eq!(list.selected_index(), 2);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("feature"));
}

#[test]
fn movement_wraps_around_the_lone_root_row_when_empty() {
    let mut list = WorktreeList::new("usagi", Vec::new());
    // Only the root row exists, so movement keeps the cursor on it.
    list.move_up();
    assert_eq!(list.selected_index(), 0);
    list.move_down();
    assert_eq!(list.selected_index(), 0);
}

#[test]
fn the_root_row_is_active_by_default() {
    let list = sample();
    assert_eq!(list.active_index(), 0);
    assert!(list.root_active());
    assert!(list.active().is_none());
}

#[test]
fn activate_selected_follows_the_cursor() {
    let mut list = sample(); // root, main, feature, fix
    list.move_down();
    list.move_down(); // cursor on "feature"
    assert_eq!(list.activate_selected(), "feature");
    assert_eq!(list.active_index(), 2);
    assert!(!list.root_active());
    // The cursor and the active row are independent afterwards.
    list.move_down(); // cursor on "fix"
    assert_eq!(list.active_index(), 2);
    assert_eq!(list.selected_index(), 3);
}

#[test]
fn activate_selected_can_return_to_the_root_row() {
    let mut list = sample();
    list.move_down(); // cursor on "main"
    list.activate_selected();
    assert!(!list.root_active());
    // Moving back to the root row and activating it returns to "root".
    list.move_up(); // cursor on the root row
    assert_eq!(list.activate_selected(), ROOT_NAME);
    assert!(list.root_active());
}

#[test]
fn activate_selected_on_an_empty_list_picks_the_root_row() {
    let mut list = WorktreeList::new("usagi", Vec::new());
    assert_eq!(list.activate_selected(), ROOT_NAME);
    assert!(list.root_active());
    assert!(list.active().is_none());
}

#[test]
fn activate_selected_records_the_session_left_as_the_jump_back_target() {
    let mut list = sample(); // root, main, feature, fix
                             // Nothing recorded until the active row first moves off the root.
    assert_eq!(list.previous_row(), None);
    list.move_down(); // cursor on "main"
    list.activate_selected(); // active main; left the root row behind
    assert_eq!(list.previous_row(), Some(0)); // jump back to the root row
    list.move_down(); // cursor on "feature"
    list.activate_selected(); // active feature; left "main" behind
    assert_eq!(list.previous_row(), Some(1)); // "main" is row 1
                                              // Re-activating the same row is a no-op focus: it must not erase the target.
    list.activate_selected();
    assert_eq!(list.previous_row(), Some(1));
}

#[test]
fn previous_row_is_none_once_the_recorded_session_no_longer_exists() {
    // A list rebuilt without the recorded session (a removed worktree) carries the
    // name forward but resolves it to no row.
    let mut list = sample();
    list.move_down();
    list.move_down(); // cursor on "feature"
    list.activate_selected(); // previous = root
    list.move_down(); // cursor on "fix"
    list.activate_selected(); // previous = "feature"
    assert_eq!(list.previous_active_name(), Some("feature"));
    assert_eq!(list.previous_row(), Some(2));
    // Carry the name onto a list that no longer has "feature".
    let mut rebuilt = WorktreeList::new("usagi", vec![worktree("main"), worktree("fix")]);
    rebuilt.set_previous_active(list.previous_active_name().map(str::to_string));
    assert_eq!(rebuilt.previous_active_name(), Some("feature"));
    assert_eq!(rebuilt.previous_row(), None);
}

#[test]
fn activate_by_name_matches_worktrees_the_root_or_reports_missing() {
    let mut list = sample(); // root, main, feature, fix
    assert!(list.activate_by_name("fix"));
    assert_eq!(list.active_index(), 3);
    // The root row is reachable by name too.
    assert!(list.activate_by_name(ROOT_NAME));
    assert_eq!(list.active_index(), 0);
    assert!(list.root_active());
    assert!(!list.activate_by_name("nope"));
    // A failed lookup leaves the active row unchanged.
    assert_eq!(list.active_index(), 0);
}

#[test]
fn select_by_name_moves_the_cursor_and_active_row_to_the_match() {
    let mut list = sample(); // root, main, feature, fix
    assert!(list.select_by_name("feature"));
    // Both the cursor and the active row land on the matched worktree.
    assert_eq!(list.selected_index(), 2);
    assert_eq!(list.active_index(), 2);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("feature"));
    // An unknown name leaves both cursors unchanged.
    assert!(!list.select_by_name("nope"));
    assert_eq!(list.selected_index(), 2);
    assert_eq!(list.active_index(), 2);
}

#[test]
fn refs_expose_the_root_row_then_worktrees_with_the_active_flag() {
    let mut list = sample();
    list.activate_by_name("feature");
    let refs = list.refs();
    assert_eq!(refs.len(), 4);
    assert_eq!(refs[0].name, ROOT_NAME);
    assert!(!refs[0].active);
    assert_eq!(refs[1].name, "main");
    assert!(!refs[1].active);
    assert_eq!(refs[2].name, "feature");
    assert!(refs[2].active);
}

#[test]
fn refs_mark_the_root_row_active_by_default() {
    let refs = sample().refs();
    assert_eq!(refs[0].name, ROOT_NAME);
    assert!(refs[0].active);
}

#[test]
fn worktree_name_falls_back_to_detached() {
    let mut detached = worktree("main");
    detached.branch = None;
    assert_eq!(worktree_name(&detached), "(detached)");
}
