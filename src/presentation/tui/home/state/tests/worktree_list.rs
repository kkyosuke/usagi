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
fn root_note_marker_defaults_off_and_toggles() {
    let mut list = WorktreeList::new("usagi", Vec::new());
    // The root row carries no note until one is recorded.
    assert!(!list.root_has_note());
    list.set_root_note_marker(true);
    assert!(list.root_has_note());
    list.set_root_note_marker(false);
    assert!(!list.root_has_note());
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
fn notes_default_to_absent_and_set_notes_aligns_to_the_worktrees() {
    let mut list = WorktreeList::with_labels(
        "usagi",
        vec![worktree("main"), worktree("feature"), worktree("fix")],
        vec![],
    );
    // A fresh list records no notes.
    assert!(!list.has_note(0));
    assert!(!list.has_note(2));
    // A shorter slice is padded with `false`; a longer one is truncated to the
    // worktree count, mirroring how labels stay aligned.
    list.set_notes(vec![true, false]);
    assert!(list.has_note(0));
    assert!(!list.has_note(1));
    assert!(!list.has_note(2)); // padded
    assert!(!list.has_note(9)); // out of range
    list.set_notes(vec![false, true, true, true]);
    assert!(!list.has_note(0));
    assert!(list.has_note(2));
    assert!(!list.has_note(3)); // truncated away
}

#[test]
fn move_down_advances_past_the_root_row_and_wraps() {
    let mut list = sample(); // root, main, feature, fix, + new session
    list.move_down();
    assert_eq!(list.selected_index(), 1);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("main"));
    list.move_down();
    list.move_down();
    assert_eq!(list.selected_index(), 3);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
    // The persistent create row sits after the last worktree.
    list.move_down();
    assert_eq!(list.selected_index(), 4);
    assert!(list.create_row_selected());
    assert!(!list.root_selected());
    // One more step wraps back to the root row.
    list.move_down();
    assert_eq!(list.selected_index(), 0);
    assert!(list.root_selected());
}

#[test]
fn move_up_wraps_from_the_root_row_to_the_bottom() {
    let mut list = sample(); // root, main, feature, fix, + new session
    list.move_up();
    assert_eq!(list.selected_index(), 4);
    assert!(list.create_row_selected());
    list.move_up();
    assert_eq!(list.selected_index(), 3);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("fix"));
}

#[test]
fn movement_wraps_around_the_lone_root_row_when_empty() {
    let mut list = WorktreeList::new("usagi", Vec::new());
    // The root and persistent create row exist even with no sessions.
    list.move_up();
    assert_eq!(list.selected_index(), 1);
    assert!(list.create_row_selected());
    list.move_down();
    assert_eq!(list.selected_index(), 0);
    assert!(list.root_selected());
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
    assert_eq!(
        list.previous_active(),
        Some(&(PathBuf::new(), "feature".to_string()))
    );
    assert_eq!(list.previous_row(), Some(2));
    // Carry the identity onto a list that no longer has "feature".
    let mut rebuilt = WorktreeList::new("usagi", vec![worktree("main"), worktree("fix")]);
    rebuilt.set_previous_active(list.previous_active().cloned());
    assert_eq!(
        rebuilt.previous_active(),
        Some(&(PathBuf::new(), "feature".to_string()))
    );
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

// --- unite mode: multiple workspace groups -------------------------------
//
// A list can stack several `WorkspaceGroup`s (one opened workspace each). The
// cursor / active row run over a flat row space concatenating every group's rows:
//   group A "wsA": [root, a1, a2]   rows 0,1,2
//   group B "wsB": [root, b1]       rows 3,4
// Single-workspace mode is just the one-group case the tests above cover.

fn united() -> WorktreeList {
    WorktreeList::from_groups(vec![
        WorkspaceGroup::new("wsA", vec![worktree("a1"), worktree("a2")]),
        WorkspaceGroup::new("wsB", vec![worktree("b1")]),
    ])
}

#[test]
fn from_groups_stacks_each_workspaces_rows() {
    let list = united();
    assert_eq!(list.group_count(), 2);
    assert_eq!(list.groups()[0].name(), "wsA");
    assert_eq!(list.groups()[1].name(), "wsB");
    // Two root rows (one per group) plus three worktrees.
    assert_eq!(list.session_count(), 5);
    // The first group is the one the legacy single-workspace accessors see.
    assert_eq!(list.workspace_name(), "wsA");
    assert_eq!(list.worktrees().len(), 2);
    // Both cursors start on the first group's root row.
    assert!(list.root_selected());
    assert_eq!(list.selected_group(), 0);
}

#[test]
fn add_group_appends_a_workspace_and_returns_its_index() {
    let mut list = WorktreeList::new("wsA", vec![worktree("a1")]);
    let idx = list.add_group(WorkspaceGroup::new("wsB", vec![worktree("b1")]));
    assert_eq!(idx, 1);
    assert_eq!(list.group_count(), 2);
    assert_eq!(list.session_count(), 4); // wsA: root,a1 + wsB: root,b1
}

#[test]
fn movement_crosses_group_boundaries_and_lands_on_each_root_row() {
    // Each expanded workspace owns its own create row now:
    //   0:wsA.root 1:a1 2:a2 3:wsA.create 4:wsB.root 5:b1 6:wsB.create
    let mut list = united();
    list.move_down();
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("a1"));
    list.move_down(); // a2
    list.move_down(); // row 3 = wsA's own create row
    assert_eq!(list.selected_index(), 3);
    assert!(list.create_row_selected());
    list.move_down(); // row 4 = wsB's root row
    assert_eq!(list.selected_index(), 4);
    assert!(list.root_selected());
    assert_eq!(list.selected_group(), 1);
    assert!(list.selected().is_none());
    list.move_down(); // row 5 = b1
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("b1"));
    assert_eq!(list.selected_group(), 1);
    list.move_down(); // row 6 = wsB's create row
    assert_eq!(list.selected_index(), 6);
    assert!(list.create_row_selected());
    list.move_down(); // wraps to the top
    assert_eq!(list.selected_index(), 0);
    list.move_up(); // wraps back to the last group's create row
    assert_eq!(list.selected_index(), 6);
    assert!(list.create_row_selected());
}

#[test]
fn activate_and_active_group_follow_the_cursor_across_groups() {
    let mut list = united(); // ... 4:wsB.root 5:b1
    list.focus_index(5); // b1 in wsB
    assert_eq!(list.activate_selected(), "b1");
    assert_eq!(list.active_group(), 1);
    assert!(!list.root_active());
    // Activating wsB's root row makes that group's root active.
    list.focus_index(4);
    assert_eq!(list.activate_selected(), ROOT_NAME);
    assert!(list.root_active());
    assert_eq!(list.active_group(), 1);
}

#[test]
fn name_lookups_find_the_first_match_across_groups() {
    let mut list = united();
    assert!(list.select_by_name("b1"));
    assert_eq!(list.selected_index(), 5);
    assert_eq!(list.active_index(), 5);
    assert!(list.activate_by_name("a2"));
    assert_eq!(list.active_index(), 2);
    // ROOT_NAME activates the first group's root row.
    assert!(list.activate_by_name(ROOT_NAME));
    assert_eq!(list.active_index(), 0);
    assert!(!list.activate_by_name("nope"));
}

/// Two groups, each holding a worktree named "shared", with distinct workspace
/// roots. Flat rows: 0 wsA.root, 1 a1, 2 shared(A), 3 wsB.root, 4 shared(B).
fn united_with_shared_names() -> WorktreeList {
    let mut a = WorkspaceGroup::new("wsA", vec![worktree("a1"), worktree("shared")]);
    a.set_root_path("/wsA");
    let mut b = WorkspaceGroup::new("wsB", vec![worktree("shared")]);
    b.set_root_path("/wsB");
    WorktreeList::from_groups(vec![a, b])
}

#[test]
fn ctrl_caret_jump_back_is_qualified_by_the_group_root() {
    let mut list = united_with_shared_names();
    // Rows (each expanded group owns a create row): wsA root0, a1(1), shared(2),
    // wsA create(3), wsB root4, shared(5), wsB create(6).
    // Leave wsB's "shared" (row 5) behind for wsA's "a1" (row 1): the jump-back
    // target is wsB's "shared" at row 5 — not wsA's same-named "shared" at row 2,
    // which a name-only lookup would wrongly return first.
    list.focus_index(5);
    list.activate_selected();
    list.focus_index(1);
    list.activate_selected();
    assert_eq!(
        list.previous_active(),
        Some(&(std::path::PathBuf::from("/wsB"), "shared".to_string()))
    );
    assert_eq!(list.previous_row(), Some(5));
    // The identity survives a carry across a rebuild that keeps both "shared" rows.
    let mut rebuilt = united_with_shared_names();
    rebuilt.set_previous_active(list.previous_active().cloned());
    assert_eq!(rebuilt.previous_row(), Some(5));
}

#[test]
fn united_totals_count_every_groups_root_and_worktrees() {
    let list = united_with_shared_names();
    // Two roots + three worktrees.
    assert_eq!(list.session_count(), 5);
    let refs = list.refs();
    assert_eq!(refs.len(), 5);
    let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["root", "a1", "shared", "root", "shared"]);
}

#[test]
fn refs_list_every_groups_root_then_its_worktrees() {
    let mut list = united();
    list.focus_index(1); // a1
    list.activate_selected();
    let refs = list.refs();
    let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["root", "a1", "a2", "root", "b1"]);
    // Only the active row (a1, index 1) is flagged active.
    assert!(refs[1].active);
    assert_eq!(refs.iter().filter(|r| r.active).count(), 1);
}

#[test]
fn set_pr_links_finds_the_row_in_any_group() {
    let mut list = united();
    let b1_root = list.groups()[1].worktrees()[0].path.clone();
    let prs = vec![crate::domain::workspace_state::PrLink {
        url: "https://example.com/pull/7".to_string(),
        number: 7,
    }];
    assert!(list.set_pr_links(&b1_root, prs.clone()));
    assert_eq!(list.groups()[1].worktrees()[0].pr, prs);
    // A path matching no row is a no-op.
    assert!(!list.set_pr_links(std::path::Path::new("/nope"), prs));
}

#[test]
fn workspace_group_from_sessions_collapses_rows_with_labels_and_notes() {
    use crate::domain::workspace_state::SessionRecord;
    let session = |name: &str, label: Option<&str>, note: Option<&str>| SessionRecord {
        label_id: None,
        agent: Default::default(),
        name: name.to_string(),
        display_name: label.map(str::to_string),
        note: note.map(str::to_string),
        root: std::path::PathBuf::from(format!("/ws/.usagi/sessions/{name}")),
        worktrees: Vec::new(),
        created_at: chrono::Utc::now(),
        last_active: None,
    };
    let group = WorkspaceGroup::from_sessions(
        "wsB",
        "/wsB",
        &[
            session("main", None, None),
            session("feat", Some("Feature"), Some("a note")),
        ],
        true,
    );
    assert_eq!(group.name(), "wsB");
    assert_eq!(group.root_path(), std::path::Path::new("/wsB"));
    assert_eq!(group.worktrees().len(), 2);
    assert_eq!(group.worktrees()[0].branch.as_deref(), Some("main"));
    assert_eq!(group.display_label(1), "Feature"); // display-name override
    assert!(!group.has_note(0));
    assert!(group.has_note(1)); // the session with a note
    assert!(group.root_has_note()); // the workspace root note marker
}

#[test]
fn workspace_group_carries_per_row_labels_and_notes() {
    let mut group = WorkspaceGroup::with_labels(
        "wsA",
        vec![worktree("a1"), worktree("a2")],
        vec![Some("One".to_string())],
    );
    assert_eq!(group.name(), "wsA");
    assert_eq!(group.display_label(0), "One"); // override
    assert_eq!(group.display_label(1), "a2"); // padded None → branch
    assert!(!group.has_note(0));
    group.set_notes(vec![true]);
    assert!(group.has_note(0));
    assert!(!group.has_note(1)); // padded false
    assert!(!group.root_has_note());
    group.set_root_note_marker(true);
    assert!(group.root_has_note());
}

// --- 統合(unite) collapse (folding a workspace) ------------------------------

#[test]
fn toggle_collapsed_folds_a_group_and_shrinks_the_nav_space() {
    // Expanded flat rows: wsA root0, a1(1), a2(2), createA(3), wsB root4, b1(5),
    // createB(6).
    let mut list = united();
    assert!(!list.is_collapsed(0));
    list.focus_index(2); // a2, inside wsA
    assert!(list.toggle_collapsed(0)); // returns the new (folded) state
    assert!(list.is_collapsed(0));
    // Folding hides rows, not sessions: the title count is unchanged.
    assert_eq!(list.session_count(), 5);
    // The cursor, which was on a now-hidden session, snapped to wsA's folded header.
    assert_eq!(list.selected_index(), 0);
    assert!(list.root_selected());
    // Folded flat rows: wsA header(0), wsB root(1), b1(2), createB(3).
    list.focus_index(2);
    assert_eq!(list.selected().unwrap().branch.as_deref(), Some("b1"));
    list.focus_index(3);
    assert!(list.create_row_selected()); // wsB's create row
                                         // Unfolding restores wsA's own rows; the cursor stays on wsB's create slot.
    assert!(!list.toggle_collapsed(0));
    assert!(!list.is_collapsed(0));
    assert!(list.create_row_selected());
    // a1 is reachable again now that wsA is expanded.
    assert!(list.select_by_name("a1"));
}

#[test]
fn toggle_collapsed_is_a_noop_for_an_out_of_range_group() {
    let mut list = united();
    assert!(!list.toggle_collapsed(9));
    assert!(!list.is_collapsed(9));
}

#[test]
fn is_create_row_is_false_past_the_end_of_the_list() {
    let list = united();
    assert!(!list.is_create_row(999)); // a row index past every group's slots
}

#[test]
fn row_in_group_of_name_yields_none_for_a_folded_group() {
    let mut list = united(); // wsA=[a1, a2], wsB=[b1]
                             // Expanded: the session resolves to its row within the group.
    assert_eq!(list.row_in_group_of_name(0, "a2"), Some(2));
    // Folded: wsA's worktrees have no navigable row, so there is none to land on.
    list.toggle_collapsed(0);
    assert_eq!(list.row_in_group_of_name(0, "a2"), None);
}

#[test]
fn refs_list_collapsed_groups_worktrees_but_never_marks_them_active() {
    let mut list = united();
    list.focus_index(5); // b1
    list.activate_selected();
    list.toggle_collapsed(0); // fold wsA (a1, a2 now hidden)
                              // b1 stays active: its ref survives the fold even though wsA's rows collapsed.
    let refs = list.refs();
    let names: Vec<&str> = refs.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(names, ["root", "a1", "a2", "root", "b1"]);
    let active: Vec<&str> = refs
        .iter()
        .filter(|r| r.active)
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(active, ["b1"]);
}

#[test]
fn set_collapsed_by_names_reapplies_folds_after_a_rebuild() {
    let mut list = united();
    let names: std::collections::HashSet<String> = ["wsB".to_string()].into_iter().collect();
    list.set_collapsed_by_names(&names);
    assert!(!list.is_collapsed(0));
    assert!(list.is_collapsed(1));
    // A name no longer present unfolds its group.
    list.set_collapsed_by_names(&std::collections::HashSet::new());
    assert!(!list.is_collapsed(1));
}

#[test]
fn folded_group_sessions_are_not_reachable_by_name_or_ctrl_caret() {
    let mut list = united();
    list.focus_index(1); // a1
    list.activate_selected(); // previous_active becomes "root"
    list.focus_index(5); // b1
    list.activate_selected(); // previous_active becomes "a1"
    assert!(list.previous_row().is_some()); // a1 is visible → resolvable
    list.toggle_collapsed(0); // fold wsA → a1 hidden
    assert!(list.previous_row().is_none()); // the Ctrl-^ target is now hidden
                                            // Name lookups skip a folded group's sessions.
    assert!(!list.select_by_name("a1"));
    assert!(list.select_by_name("b1"));
}
