//! The background loading-tab tracker: [`HomeState::begin_pending_pane`] and the
//! two-phase poll/animation helpers the event loop drives. The full attach flow is
//! covered by the event-loop `background_tab` tests; these pin the state
//! transitions between the Resolving (placeholder) and Starting (real tab) phases.

use super::*;
use std::path::PathBuf;

#[test]
fn begin_starts_in_the_resolving_phase_with_a_placeholder() {
    let mut state = state();
    state.begin_pending_pane(PathBuf::from("/r/feat"), 3, "terminal".to_string());
    let pending = state.pending_pane().expect("a launch is pending");
    assert_eq!(pending.dir(), PathBuf::from("/r/feat"));
    assert_eq!(pending.interaction_epoch(), 3);
    assert_eq!(
        pending.placeholder(),
        Some("terminal"),
        "resolving shows a placeholder chip"
    );
    // The chip has no resolved tab index yet, so nothing animates.
    assert_eq!(state.loading_tab(), None);
}

#[test]
fn advancing_while_resolving_places_the_placeholder_chip_and_keeps_it() {
    let mut state = state();
    state.begin_pending_pane(PathBuf::from("/r/feat"), 0, "Claude".to_string());
    state.advance_pending_pane(2, true); // placeholder appended at strip index 2
    assert_eq!(
        state.loading_tab(),
        Some((2, 1)),
        "tab 2, frame advanced to 1"
    );
    assert_eq!(
        state.pending_pane().and_then(|p| p.placeholder()),
        Some("Claude"),
        "still resolving, so the placeholder stays",
    );
}

#[test]
fn advancing_when_started_drops_the_placeholder_for_the_real_tab() {
    let mut state = state();
    state.begin_pending_pane(PathBuf::from("/r/feat"), 0, "terminal".to_string());
    state.advance_pending_pane(1, false); // the pane spawned at pool tab 1
    assert_eq!(state.loading_tab(), Some((1, 1)));
    assert_eq!(
        state.pending_pane().and_then(|p| p.placeholder()),
        None,
        "once started, the pane carries its own tab label",
    );
}

#[test]
fn advancing_with_nothing_pending_is_a_noop() {
    let mut state = state();
    state.advance_pending_pane(0, true);
    assert!(state.pending_pane().is_none());
    assert_eq!(state.loading_tab(), None);
}

#[test]
fn clearing_returns_the_tracker_and_empties_it() {
    let mut state = state();
    state.begin_pending_pane(PathBuf::from("/r/feat"), 0, "terminal".to_string());
    let cleared = state.clear_pending_pane().expect("the tracker is returned");
    assert_eq!(cleared.dir(), PathBuf::from("/r/feat"));
    assert!(state.pending_pane().is_none());
    // Clearing again with nothing pending yields nothing.
    assert!(state.clear_pending_pane().is_none());
}
