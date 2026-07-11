//! 選択 (Overview) manual-status label cycling ([`HomeState::cycle_selected_label`]
//! / [`select_label_index`](HomeState::select_label_index) /
//! [`clear_selected_label`](HomeState::clear_selected_label)) and row resolution
//! ([`HomeState::row_label`]).

use super::*;
use crate::domain::settings::{LabelColor, SessionLabelDef, SessionLabelMaster};

fn def(id: &str, name: &str) -> SessionLabelDef {
    SessionLabelDef {
        id: id.to_string(),
        name: name.to_string(),
        color: LabelColor::Gray,
        icon: None,
    }
}

fn master() -> SessionLabelMaster {
    SessionLabelMaster {
        labels: vec![
            def("todo", "Todo"),
            def("doing", "Doing"),
            def("done", "Done"),
        ],
    }
}

/// A state whose first session (`alpha`) carries `current` as its label, with the
/// cursor moved onto it and the given master installed.
fn state_with_label(current: Option<&str>, master: SessionLabelMaster) -> HomeState {
    let mut alpha = session_record("alpha", 1);
    alpha.label_id = current.map(str::to_string);
    let mut state = state();
    state.set_label_master(master);
    state.restore_sessions(vec![alpha, session_record("beta", 1)]);
    state.overview_move_down(); // root -> alpha
    state
}

#[test]
fn cycle_forward_rings_through_the_unset_slot_and_every_label() {
    // Unset → the first label.
    let s = state_with_label(None, master());
    assert_eq!(
        s.cycle_selected_label(true),
        Some(("alpha".to_string(), Some("todo".to_string())))
    );
    // A middle label → the next one.
    let s = state_with_label(Some("todo"), master());
    assert_eq!(
        s.cycle_selected_label(true),
        Some(("alpha".to_string(), Some("doing".to_string())))
    );
    // The last label → back to the unset slot.
    let s = state_with_label(Some("done"), master());
    assert_eq!(
        s.cycle_selected_label(true),
        Some(("alpha".to_string(), None))
    );
}

#[test]
fn cycle_backward_rings_the_other_way() {
    // Unset → the last label.
    let s = state_with_label(None, master());
    assert_eq!(
        s.cycle_selected_label(false),
        Some(("alpha".to_string(), Some("done".to_string())))
    );
    // The first label → back to the unset slot.
    let s = state_with_label(Some("todo"), master());
    assert_eq!(
        s.cycle_selected_label(false),
        Some(("alpha".to_string(), None))
    );
}

#[test]
fn cycle_treats_a_stale_id_as_unset() {
    // An id no longer in the master (a since-removed label) rings from the unset
    // slot, so forward lands on the first label.
    let s = state_with_label(Some("ghost"), master());
    assert_eq!(
        s.cycle_selected_label(true),
        Some(("alpha".to_string(), Some("todo".to_string())))
    );
}

#[test]
fn cycle_is_a_no_op_on_the_root_row_and_with_no_labels() {
    // Cursor left on the root row (no `overview_move_down`).
    let mut root = state();
    root.set_label_master(master());
    root.restore_sessions(vec![session_record("alpha", 1)]);
    assert_eq!(root.cycle_selected_label(true), None);

    // A session row but an empty master → the feature is dormant.
    let empty = state_with_label(None, SessionLabelMaster { labels: vec![] });
    assert_eq!(empty.cycle_selected_label(true), None);
}

#[test]
fn select_label_index_picks_the_nth_label_and_ignores_out_of_range() {
    let s = state_with_label(None, master());
    // Digit `2` → index 1 → the second label.
    assert_eq!(
        s.select_label_index(1),
        Some(("alpha".to_string(), Some("doing".to_string())))
    );
    // Past the end → a no-op.
    assert_eq!(s.select_label_index(9), None);
    // Selecting the label already set is a no-op (no needless write).
    let already = state_with_label(Some("doing"), master());
    assert_eq!(already.select_label_index(1), None);
}

#[test]
fn select_label_index_is_a_no_op_off_a_session() {
    let mut root = state();
    root.set_label_master(master());
    root.restore_sessions(vec![session_record("alpha", 1)]);
    assert_eq!(root.select_label_index(0), None);
}

#[test]
fn clear_selected_label_clears_a_set_label_and_no_ops_otherwise() {
    // A session carrying a label → cleared.
    let set = state_with_label(Some("todo"), master());
    assert_eq!(
        set.clear_selected_label(),
        Some(("alpha".to_string(), None))
    );
    // Already unset → nothing to write.
    let unset = state_with_label(None, master());
    assert_eq!(unset.clear_selected_label(), None);
    // On the root row → a no-op.
    let mut root = state();
    root.restore_sessions(vec![session_record("alpha", 1)]);
    assert_eq!(root.clear_selected_label(), None);
}

#[test]
fn row_label_resolves_the_first_rows_label_against_the_master() {
    // A set, resolvable id → the matching def.
    let set = state_with_label(Some("doing"), master());
    assert_eq!(set.row_label(0).map(|d| d.name.as_str()), Some("Doing"));
    // An unset row → None.
    let unset = state_with_label(None, master());
    assert_eq!(unset.row_label(0), None);
    // A stale id (not in the master) reads as unset.
    let stale = state_with_label(Some("ghost"), master());
    assert_eq!(stale.row_label(0), None);
}
