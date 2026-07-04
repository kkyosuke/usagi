//! The background loading-tab tracker: [`HomeState::begin_pending_pane`] and the
//! poll/animation helpers the event loop drives. The full attach flow is covered
//! by the event-loop `background_tab` tests; these pin the state transitions.

use super::*;
use std::path::PathBuf;

#[test]
fn begin_records_the_pending_pane_un_placed_and_un_animated() {
    let mut state = state();
    state.begin_pending_pane(PathBuf::from("/r/feat"), 7, 3);
    let pending = state.pending_pane().expect("a pane is pending");
    assert_eq!(pending.dir(), PathBuf::from("/r/feat"));
    assert_eq!(pending.pane_id(), 7);
    assert_eq!(pending.interaction_epoch(), 3);
    // The chip has no resolved tab index yet, so nothing animates.
    assert_eq!(state.loading_tab(), None);
}

#[test]
fn stepping_advances_the_frame_and_places_the_chip() {
    let mut state = state();
    state.begin_pending_pane(PathBuf::from("/r/feat"), 7, 0);
    state.step_pending_pane(2);
    assert_eq!(
        state.loading_tab(),
        Some((2, 1)),
        "tab 2, frame advanced to 1"
    );
    state.step_pending_pane(2);
    assert_eq!(
        state.loading_tab(),
        Some((2, 2)),
        "the next poll advances the frame again"
    );
}

#[test]
fn stepping_with_nothing_pending_is_a_noop() {
    let mut state = state();
    state.step_pending_pane(0);
    assert!(state.pending_pane().is_none());
    assert_eq!(state.loading_tab(), None);
}

#[test]
fn clearing_returns_the_tracker_and_empties_it() {
    let mut state = state();
    state.begin_pending_pane(PathBuf::from("/r/feat"), 7, 0);
    let cleared = state.clear_pending_pane().expect("the tracker is returned");
    assert_eq!(cleared.pane_id(), 7);
    assert!(state.pending_pane().is_none());
    // Clearing again with nothing pending yields nothing.
    assert!(state.clear_pending_pane().is_none());
}
